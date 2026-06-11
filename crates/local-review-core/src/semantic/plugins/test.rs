//! Test-file semantic extractor for Jest/Jasmine/Vitest/Mocha patterns.
//!
//! Extracts `describe`, `it`, and `test` blocks as entities using their string
//! label as the entity name. Scope-chains nest naturally: a suite `"A"` with
//! case `"b"` inside produces scope chains `["A"]` and `["A", "b"]`
//! respectively, so the Container Rule suppresses the suite when only a case
//! changed.
//!
//! Dispatched before extension lookup whenever the filename contains `.spec.`
//! or `.test.`. Uses the TypeScript TSX grammar (a superset of TS/JS/TSX).

#![cfg(feature = "lang-typescript")]

use tree_sitter::{Node, Parser};

use crate::semantic::entity::{EntityKind, RawEntity};
use crate::semantic::extractor::{
    body_hash, build_entity_id, content_hash, sig_hash, ExtractError, ExtractResult,
    SemanticExtractor,
};
use crate::util::strip_controls;

// ── Known test-framework call names ───────────────────────────────────────────

fn test_call_kind(name: &str) -> Option<EntityKind> {
    match name {
        "describe" | "xdescribe" | "fdescribe" | "suite" | "context" => Some(EntityKind::TestSuite),
        "it" | "xit" | "fit" | "test" | "xtest" | "ftest" | "specify" => Some(EntityKind::TestCase),
        _ => None,
    }
}

// ── AST helpers ───────────────────────────────────────────────────────────────

/// Return the base identifier name from a call's function node.
///
/// Handles plain identifiers (`describe`) and member expressions
/// (`describe.only`, `it.skip`, `test.each(...)`).
fn callee_base_name<'t>(function_node: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    match function_node.kind() {
        "identifier" => function_node.utf8_text(src).ok(),
        "member_expression" => function_node
            .child_by_field_name("object")
            .and_then(|n| n.utf8_text(src).ok()),
        _ => None,
    }
}

/// Extract the first string-literal argument text from an `arguments` node,
/// stripping surrounding quote characters.
fn first_string_label(arguments: Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..arguments.child_count() {
        let Ok(i32) = u32::try_from(i) else { continue };
        let Some(child) = arguments.child(i32) else {
            continue;
        };
        if child.kind() == "string" {
            let raw = child.utf8_text(src).unwrap_or("").trim();
            // Strip the surrounding ' or " character(s).
            let inner = raw
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(raw);
            let cleaned = strip_controls(inner.trim());
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

/// Find the callback argument (`arrow_function` or `function_expression`) inside an
/// `arguments` node.
fn find_callback(arguments: Node<'_>) -> Option<Node<'_>> {
    for i in 0..arguments.child_count() {
        let Ok(i32) = u32::try_from(i) else { continue };
        let Some(child) = arguments.child(i32) else {
            continue;
        };
        if matches!(
            child.kind(),
            "arrow_function" | "function" | "function_expression"
        ) {
            return Some(child);
        }
    }
    None
}

// ── Recursive walker ──────────────────────────────────────────────────────────

struct Collector<'t> {
    src: &'t [u8],
    file_path: String,
    entities: Vec<RawEntity>,
}

impl<'t> Collector<'t> {
    fn new(src: &'t [u8], file_path: &str) -> Self {
        Self {
            src,
            file_path: file_path.to_owned(),
            entities: Vec::new(),
        }
    }

    /// Walk `node`, emitting entities for test-framework call expressions.
    fn walk(&mut self, node: Node<'t>, parent_scope: &str) {
        for i in 0..node.child_count() {
            let Ok(i32) = u32::try_from(i) else { continue };
            let Some(child) = node.child(i32) else {
                continue;
            };
            if child.kind() == "call_expression" && self.try_extract(child, parent_scope) {
                // Recursion into the callback is handled inside try_extract.
                continue;
            }
            self.walk(child, parent_scope);
        }
    }

    /// Attempt to extract a test-framework entity from a `call_expression`.
    ///
    /// Returns `true` if the call was recognised (and the plugin recurses into
    /// its callback internally); `false` if it should be traversed normally.
    fn try_extract(&mut self, call: Node<'t>, parent_scope: &str) -> bool {
        let Some(function_node) = call.child_by_field_name("function") else {
            return false;
        };
        let Some(callee) = callee_base_name(function_node, self.src) else {
            return false;
        };
        let Some(kind) = test_call_kind(callee) else {
            return false;
        };
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        let Some(label) = first_string_label(arguments, self.src) else {
            return false;
        };

        let scope = if parent_scope.is_empty() {
            label.clone()
        } else {
            format!("{parent_scope}::{label}")
        };

        let entity_id = build_entity_id(&self.file_path, &scope, 0);
        let content = call.utf8_text(self.src).unwrap_or("").to_owned();
        let start_line = u32::try_from(call.start_position().row + 1).unwrap_or(1);
        let end_line = u32::try_from(call.end_position().row + 1).unwrap_or(start_line);
        let ch = content_hash(&content);
        let sh = sig_hash(&content);
        let bh = body_hash(&content);

        self.entities.push(RawEntity {
            id: entity_id,
            scope: scope.clone(),
            kind,
            start_line,
            end_line,
            content,
            content_hash: ch,
            sig_hash: sh,
            body_hash: bh,
            file_path: self.file_path.clone(),
        });

        // Recurse into the callback body for nested describe/it/test blocks.
        if let Some(callback) = find_callback(arguments) {
            if let Some(body) = callback.child_by_field_name("body") {
                self.walk(body, &scope);
            }
        }

        true
    }
}

// ── Ordinal assignment ─────────────────────────────────────────────────────────

fn assign_ordinals(entities: &mut [RawEntity]) {
    let keys: Vec<_> = entities
        .iter()
        .map(|e| {
            (
                e.id.scope_chain.clone(),
                e.id.signature_key.clone(),
                e.start_line,
            )
        })
        .collect();
    let mut ordinals = vec![0u32; keys.len()];
    crate::semantic::entity_id::assign_ordinals(&keys, &mut ordinals);
    for (entity, ord) in entities.iter_mut().zip(ordinals.iter()) {
        entity.id.ordinal = *ord;
    }
}

// ── Plugin ─────────────────────────────────────────────────────────────────────

pub(crate) struct TestPlugin;

impl SemanticExtractor for TestPlugin {
    fn id(&self) -> &'static str {
        "test"
    }

    fn extensions(&self) -> &[&str] {
        // Extensions are a fallback; filename_patterns takes priority.
        &[]
    }

    fn filename_patterns(&self) -> &[&str] {
        &[".spec.", ".test."]
    }

    fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .map_err(|e| ExtractError::ParserInit {
                detail: format!("tree-sitter-typescript init failed for {file_path}: {e}"),
            })?;
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| ExtractError::ParserInit {
                detail: format!("tree-sitter-typescript parse returned None for {file_path}"),
            })?;

        let mut collector = Collector::new(content.as_bytes(), file_path);
        let root = tree.root_node();
        // Use a cursor for the top-level walk — tree lifetime is local here.
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            collector.walk(child, "");
        }

        assign_ordinals(&mut collector.entities);
        Ok(collector.entities)
    }
}
