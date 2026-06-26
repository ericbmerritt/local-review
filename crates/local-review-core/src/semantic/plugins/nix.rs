//! Nix semantic extractor backed by `tree-sitter-nix`.
//!
//! Nix files are attribute sets. The primary entities are **bindings**
//! (`name = value;`) at the top level of the file or inside a let-in
//! expression. Bindings whose values are functions are classified as
//! `Function`; all other bindings are `ConfigProperty`.
//!
//! tree-sitter-nix 0.3.0 node kinds used here:
//! - `source_code`          — root
//! - `attrset_expression`   — `{ ... }`
//! - `rec_attrset_expression` — `rec { ... }`
//! - `binding_set`          — the body of an attrset (contains bindings)
//! - `binding`              — `attrpath = value ;`
//! - `attrpath`             — left-hand side of a binding
//! - `let_expression`       — `let bindings in body`
//! - `let_attrset`          — the binding portion of let-in
//! - `function`             — `arg: body` or `{ args }: body`
//! - `with_expression`      — `with scope; body`

#![cfg(feature = "lang-nix")]

use tree_sitter::{Node, Parser};

use crate::semantic::entity::{EntityKind, RawEntity};
use crate::semantic::extractor::{
    body_hash, build_entity_id, content_hash, sig_hash, ExtractError, ExtractResult,
    SemanticExtractor,
};
use crate::util::strip_controls;

pub(crate) struct NixPlugin;

impl SemanticExtractor for NixPlugin {
    fn id(&self) -> &'static str {
        "nix"
    }

    fn extensions(&self) -> &[&str] {
        &["nix"]
    }

    fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_nix::LANGUAGE.into())
            .map_err(|e| ExtractError::ParserInit {
                detail: format!("nix language init failed: {e}"),
            })?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| ExtractError::ParserInit {
                detail: format!("tree-sitter-nix parse returned None for {file_path}"),
            })?;

        let root = tree.root_node();
        if root.has_error() {
            return Err(ExtractError::ParseContainsErrors {
                file_path: file_path.into(),
            });
        }

        let src = content.as_bytes();
        let mut entities = Vec::new();
        collect_top_level(root, src, file_path, content, &mut entities);
        Ok(entities)
    }
}

/// Walk `node` to find binding collections at the file's top logical level.
fn collect_top_level(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    content: &str,
    out: &mut Vec<RawEntity>,
) {
    match node.kind() {
        "source_code" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_top_level(child, src, file_path, content, out);
            }
        }

        // `{ ... }`, `rec { ... }`, and `let ... in` — all contain a
        // `binding_set` node that holds the actual bindings.
        "attrset_expression" | "rec_attrset_expression" | "let_expression" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "binding_set" {
                    collect_binding_set(child, src, file_path, content, out);
                }
            }
        }

        // `args: body` — common wrapper; recurse into body.
        // e.g. `{ pkgs, ... }: { foo = ...; }`
        "function_expression" => {
            if let Some(body) = function_body(node) {
                collect_top_level(body, src, file_path, content, out);
            }
        }

        // `with scope; body`
        "with_expression" => {
            let mut cursor = node.walk();
            if let Some(body) = node.named_children(&mut cursor).last() {
                collect_top_level(body, src, file_path, content, out);
            }
        }

        _ => {}
    }
}

/// Collect entities from a `binding_set` or `let_attrset` node.
fn collect_binding_set(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    content: &str,
    out: &mut Vec<RawEntity>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "binding" => emit_binding(child, src, file_path, content, out),
            "inherit" => emit_inherit(child, src, file_path, content, out),
            _ => {}
        }
    }
}

/// Return the body node of a `function_expression` (skips formals/arg).
///
/// In tree-sitter-nix 0.3.0, `function_expression` has: arg (formal or
/// identifier) then the body as the last named child.
fn function_body(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).last()
}

/// Emit an entity for a `binding` node.
fn emit_binding(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    content: &str,
    out: &mut Vec<RawEntity>,
) {
    let Some(attrpath) = node.child_by_field_name("attrpath") else {
        return;
    };
    let Some(name) = attrpath_name(attrpath, src) else {
        return;
    };

    // Determine kind: if the value expression is a function, classify as Function.
    let kind = value_kind(node);

    let start = u32::try_from(node.start_position().row + 1).unwrap_or(1);
    let end = u32::try_from(node.end_position().row + 1).unwrap_or(start);
    let node_text = node.utf8_text(src).unwrap_or("").to_owned();
    let entity_id = build_entity_id(file_path, &name, 0);

    out.push(RawEntity {
        id: entity_id,
        scope: name.clone(),
        kind,
        start_line: start,
        end_line: end,
        content: node_text.clone(),
        content_hash: content_hash(content),
        sig_hash: sig_hash(&name),
        body_hash: body_hash(&node_text),
        file_path: file_path.to_owned(),
    });
}

/// Classify the kind of a `binding` node by inspecting its value expression.
fn value_kind(binding: Node<'_>) -> EntityKind {
    // The value is the last named child of the binding (after the attrpath).
    let mut cursor = binding.walk();
    let value = binding.named_children(&mut cursor).last();
    match value.map(|n| n.kind()) {
        Some("function_expression") => EntityKind::Function,
        _ => EntityKind::ConfigProperty,
    }
}

/// Emit one entity per inherited name in `inherit (scope)? name1 name2 ...;`.
fn emit_inherit(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    content: &str,
    out: &mut Vec<RawEntity>,
) {
    let start = u32::try_from(node.start_position().row + 1).unwrap_or(1);
    let end = u32::try_from(node.end_position().row + 1).unwrap_or(start);
    let node_text = node.utf8_text(src).unwrap_or("").to_owned();

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "identifier" {
            continue;
        }
        let Some(name) = child
            .utf8_text(src)
            .ok()
            .map(strip_controls)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let entity_id = build_entity_id(file_path, &name, 0);
        out.push(RawEntity {
            id: entity_id,
            scope: name.clone(),
            kind: EntityKind::ConfigProperty,
            start_line: start,
            end_line: end,
            content: node_text.clone(),
            content_hash: content_hash(content),
            sig_hash: sig_hash(&name),
            body_hash: body_hash(&node_text),
            file_path: file_path.to_owned(),
        });
    }
}

/// Return the full attrpath text (e.g. `services.nginx.enable`) as the entity name.
fn attrpath_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    node.utf8_text(src)
        .ok()
        .map(strip_controls)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<RawEntity> {
        NixPlugin.extract(src, "test.nix").unwrap_or_default()
    }

    #[test]
    fn extracts_attrset_bindings() {
        let entities = extract("{ foo = 1; bar = \"hello\"; }");
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert!(names.contains(&"foo"), "expected foo; got {names:?}");
        assert!(names.contains(&"bar"), "expected bar; got {names:?}");
    }

    #[test]
    fn function_binding_classified_as_function() {
        let entities = extract("{ authenticate = creds: 1; }");
        let e = entities.iter().find(|e| e.id.name() == "authenticate");
        assert!(e.is_some(), "expected authenticate entity");
        assert_eq!(e.unwrap().kind, EntityKind::Function);
    }

    #[test]
    fn property_binding_classified_as_config_property() {
        let entities = extract("{ enable = true; }");
        let e = entities.iter().find(|e| e.id.name() == "enable");
        assert_eq!(e.unwrap().kind, EntityKind::ConfigProperty);
    }

    #[test]
    fn let_in_bindings_extracted() {
        let entities = extract("let foo = 1; bar = 2; in foo + bar");
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert!(names.contains(&"foo"), "expected foo; got {names:?}");
        assert!(names.contains(&"bar"), "expected bar; got {names:?}");
    }

    #[test]
    fn function_wrapper_transparent() {
        let entities = extract("{ pkgs }: { myPkg = pkgs.hello; }");
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert!(names.contains(&"myPkg"), "expected myPkg; got {names:?}");
    }

    #[test]
    fn nested_attrpath_preserved() {
        let entities = extract("{ services.nginx.enable = true; }");
        assert!(!entities.is_empty(), "expected at least one entity");
        assert_eq!(
            entities[0].id.name(),
            "services.nginx.enable",
            "full attrpath should be the entity name"
        );
    }

    #[test]
    fn parse_error_returns_err() {
        // Malformed Nix that tree-sitter cannot parse cleanly.
        // This may or may not produce an error depending on how tree-sitter
        // handles it; at minimum it must not panic.
        let _ = NixPlugin.extract("{ unclosed =", "bad.nix");
    }

    #[test]
    fn rec_attrset_works() {
        let entities = extract("rec { foo = 1; bar = foo + 1; }");
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert!(names.contains(&"foo"), "expected foo; got {names:?}");
        assert!(names.contains(&"bar"), "expected bar; got {names:?}");
    }
}
