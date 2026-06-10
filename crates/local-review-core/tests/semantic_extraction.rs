//! Integration tests for the semantic extraction layer.
//!
//! Each test validates a specific Done-When criterion from the Phase 1 spec.

use local_review_core::semantic::{create_default_registry, diff_entities, ChangeType, EntityKind};

// ── Helper ────────────────────────────────────────────────────────────────────

fn registry() -> local_review_core::semantic::ExtractorRegistry {
    create_default_registry()
}

// ── Rust ──────────────────────────────────────────────────────────────────────

#[test]
fn rust_extracts_top_level_functions() {
    let src = include_str!("semantic-golden/rust/01-functions.rs");
    let r = registry();
    let entities = r.extract(src, "auth.rs").expect("extraction must succeed");
    let names: Vec<&str> = entities
        .iter()
        .filter(|e| e.kind == EntityKind::Function)
        .map(|e| e.id_str.as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("authenticate")),
        "must find authenticate; got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("validate_token")),
        "must find validate_token; got: {names:?}"
    );
}

#[test]
fn rust_extracts_impl_methods() {
    let src = include_str!("semantic-golden/rust/01-functions.rs");
    let r = registry();
    let entities = r.extract(src, "auth.rs").expect("extraction must succeed");
    let has_refresh = entities
        .iter()
        .any(|e| e.kind == EntityKind::Function && e.id_str.contains("refresh"));
    assert!(has_refresh, "must find refresh method inside impl Session");
}

#[test]
fn rust_parse_error_nodes_returns_err() {
    let bad_rust = "fn broken( { this is not valid rust !!!";
    let r = registry();
    let result = r.extract(bad_rust, "broken.rs");
    assert!(
        result.is_err(),
        "file with ERROR nodes must return Err, not a partial list"
    );
}

#[test]
fn rust_container_rule_suppresses_unchanged_struct() {
    let before = "struct Foo { x: i32 }\nimpl Foo {\n    pub fn bar(&self) -> i32 { self.x }\n}";
    let after = "struct Foo { x: i32 }\nimpl Foo {\n    pub fn bar(&self) -> i32 { self.x + 1 }\n}";
    let r = registry();
    let b = r.extract(before, "foo.rs").expect("before must extract");
    let a = r.extract(after, "foo.rs").expect("after must extract");
    let changes = diff_entities(&b, &a);
    let kinds: Vec<EntityKind> = changes.iter().map(|e| e.kind).collect();
    // bar method should appear (it changed), struct and impl should not
    // (their declarations did not change)
    assert!(
        changes
            .iter()
            .any(|e| e.kind == EntityKind::Function && e.change == ChangeType::Modified),
        "method change must appear; changes: {kinds:?}"
    );
    assert!(
        !changes.iter().any(|e| e.kind == EntityKind::Struct),
        "unchanged struct declaration must not appear; changes: {kinds:?}"
    );
}

// ── Python ────────────────────────────────────────────────────────────────────

#[test]
fn python_extracts_functions_and_class() {
    let src = include_str!("semantic-golden/python/01-functions.py");
    let r = registry();
    let entities = r.extract(src, "auth.py").expect("extraction must succeed");
    let has_authenticate = entities.iter().any(|e| e.id_str.contains("authenticate"));
    let has_session = entities.iter().any(|e| e.id_str.contains("Session"));
    assert!(has_authenticate, "must find authenticate in python");
    assert!(has_session, "must find Session class in python");
}

// ── PostgreSQL ────────────────────────────────────────────────────────────────

#[test]
fn postgres_extracts_table_and_function() {
    let src = include_str!("semantic-golden/postgres/01-ddl.sql");
    let r = registry();
    let entities = r
        .extract(src, "schema.sql")
        .expect("postgres extraction must succeed");
    let kinds: Vec<EntityKind> = entities.iter().map(|e| e.kind).collect();
    assert!(
        kinds.contains(&EntityKind::Table),
        "must extract Table entity; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&EntityKind::Function),
        "must extract Function entity; got: {kinds:?}"
    );
}

// ── Jaccard matching ──────────────────────────────────────────────────────────

#[test]
fn jaccard_links_renamed_function_across_before_after() {
    // A function body stays the same, only the name changes.
    let before = "
pub fn authenticate(user: &str, pass: &str) -> bool {
    // check credentials here
    let valid = !user.is_empty() && !pass.is_empty();
    valid && user.len() < 50 && pass.len() < 100
}
";
    let after = "
pub fn verify_credentials(user: &str, pass: &str) -> bool {
    // check credentials here
    let valid = !user.is_empty() && !pass.is_empty();
    valid && user.len() < 50 && pass.len() < 100
}
";
    let r = registry();
    let b = r.extract(before, "auth.rs").expect("before must extract");
    let a = r.extract(after, "auth.rs").expect("after must extract");
    let changes = diff_entities(&b, &a);
    // The function should be detected as Modified (body same, name changed)
    // rather than Deleted + Added.
    let added = changes
        .iter()
        .filter(|e| e.change == ChangeType::Added)
        .count();
    let deleted = changes
        .iter()
        .filter(|e| e.change == ChangeType::Deleted)
        .count();
    let modified = changes
        .iter()
        .filter(|e| e.change == ChangeType::Modified)
        .count();
    assert!(
        modified > 0 || (added == 0 && deleted == 0),
        "Jaccard must link the renamed function; \
         got {added} added, {deleted} deleted, {modified} modified"
    );
}

#[test]
fn tiny_entity_below_token_threshold_not_fuzzy_matched_cross_file() {
    // Single-line trivial getter — below the 20-token threshold for Jaccard.
    // When the function has DIFFERENT (but structurally similar) bodies in two
    // different files, Jaccard must NOT match them (noise mitigation).
    // Note: identical content IS matched by hash (correct behavior); this test
    // exercises Jaccard-specific noise mitigation with non-identical content.
    let before = "pub fn get_id(&self) -> &str { &self.id }";
    let after = "pub fn get_name(&self) -> &str { &self.name }"; // different content, similar shape
    let r = registry();
    let b = r.extract(before, "before.rs").expect("before must extract");
    let a = r.extract(after, "after.rs").expect("after must extract");
    let changes = diff_entities(&b, &a);
    // With Jaccard noise mitigation, the tiny entities must not cross-file match.
    // They should appear as Deleted + Added, not Moved.
    let moved = changes
        .iter()
        .filter(|e| e.change == ChangeType::Moved)
        .count();
    assert_eq!(
        moved, 0,
        "tiny entity below token threshold must not fuzzy-match cross-file; \
         changes: {changes:?}"
    );
    // Should be 1 added + 1 deleted
    let added = changes
        .iter()
        .filter(|e| e.change == ChangeType::Added)
        .count();
    let deleted = changes
        .iter()
        .filter(|e| e.change == ChangeType::Deleted)
        .count();
    assert_eq!(added, 1, "must have 1 added");
    assert_eq!(deleted, 1, "must have 1 deleted");
}

// ── Unsupported language ──────────────────────────────────────────────────────

#[test]
fn unsupported_extension_returns_err() {
    let r = registry();
    let result = r.extract("hello world", "script.rb");
    assert!(
        result.is_err(),
        "unsupported language must return Err (fallback row in UI)"
    );
}

// ── Content hash ──────────────────────────────────────────────────────────────

#[test]
fn identical_content_produces_same_hash() {
    let src = "pub fn foo() -> i32 { 42 }";
    let r = registry();
    let e1 = r.extract(src, "a.rs").expect("extract a");
    let e2 = r.extract(src, "a.rs").expect("extract b");
    assert_eq!(
        e1.len(),
        e2.len(),
        "same input must produce same entity count"
    );
    for (a, b) in e1.iter().zip(e2.iter()) {
        assert_eq!(
            a.content_hash, b.content_hash,
            "same input must produce same hash"
        );
    }
}

#[test]
fn strip_controls_applied_to_entity_ids() {
    let src = "pub fn foo() -> i32 { 42 }";
    let r = registry();
    let entities = r.extract(src, "test\x1b[31mfile.rs").expect("extract");
    for e in &entities {
        assert!(
            !e.id_str.contains('\x1b'),
            "ESC control character must be stripped from entity id: {}",
            e.id_str
        );
    }
}
