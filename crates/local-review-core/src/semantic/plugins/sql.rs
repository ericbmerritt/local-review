//! `PostgreSQL` semantic extractor backed by `tree-sitter-postgres`.
//!
//! Extracts DDL-level entities: tables, functions, views, indexes, types,
//! triggers, RLS policies, schemas, and extensions.
//!
//! Node kind names match tree-sitter-postgres grammar v1.1.x. Statement
//! structure: `source_file → toplevel_stmt → stmt → <DDLNode>`.

#![cfg(feature = "lang-postgres")]

use tree_sitter::{Node, Parser};

use crate::semantic::entity::{EntityKind, RawEntity};
use crate::semantic::extractor::{
    body_hash, build_id_str, content_hash, sig_hash, ExtractError, ExtractResult, SemanticExtractor,
};
use crate::util::strip_controls;

// ── Entity kind dispatch ───────────────────────────────────────────────────────

fn ddl_kind(node_kind: &str) -> Option<EntityKind> {
    match node_kind {
        "CreateStmt" | "AlterTableStmt" => Some(EntityKind::Table),
        "CreateFunctionStmt" => Some(EntityKind::Function),
        "ViewStmt" => Some(EntityKind::View),
        "IndexStmt" => Some(EntityKind::Index),
        "DefineStmt" => Some(EntityKind::Type),
        "CreateTrigStmt" => Some(EntityKind::Trigger),
        "CreatePolicyStmt" => Some(EntityKind::Policy),
        "CreateSchemaStmt" => Some(EntityKind::Schema),
        "CreateExtensionStmt" => Some(EntityKind::Extension),
        _ => None,
    }
}

// ── Name extraction ────────────────────────────────────────────────────────────

/// Recursively find the first `identifier` leaf within `node`.
fn first_identifier<'t>(node: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    if node.kind() == "identifier" {
        return node.utf8_text(src).ok();
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = first_identifier(child, src) {
            return Some(name);
        }
    }
    None
}

fn extract_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    first_identifier(node, src)
        .map(strip_controls)
        .filter(|s| !s.is_empty())
}

// ── Error node detection ───────────────────────────────────────────────────────

fn has_error(root: Node<'_>) -> bool {
    if root.is_error() || root.is_missing() {
        return true;
    }
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if has_error(child) {
            return true;
        }
    }
    false
}

// ── Statement collection ───────────────────────────────────────────────────────

fn collect_stmt(stmt_node: Node<'_>, src: &[u8], file_path: &str, entities: &mut Vec<RawEntity>) {
    let mut cursor = stmt_node.walk();
    for ddl_node in stmt_node.named_children(&mut cursor) {
        let Some(ek) = ddl_kind(ddl_node.kind()) else {
            continue;
        };
        let Some(name) = extract_name(ddl_node, src) else {
            continue;
        };
        let id_str = build_id_str(file_path, ddl_node.kind(), &name);
        let content = ddl_node.utf8_text(src).unwrap_or("").to_owned();
        let start_line = u32::try_from(ddl_node.start_position().row + 1).unwrap_or(1);
        let end_line = u32::try_from(ddl_node.end_position().row + 1).unwrap_or(start_line);
        entities.push(RawEntity {
            id_str,
            scope: name.clone(),
            kind: ek,
            start_line,
            end_line,
            content_hash: content_hash(&content),
            sig_hash: sig_hash(&content),
            body_hash: body_hash(&content),
            content,
            file_path: file_path.to_owned(),
        });
    }
}

// ── Extractor impl ─────────────────────────────────────────────────────────────

pub(crate) struct SqlPlugin;

impl SemanticExtractor for SqlPlugin {
    fn id(&self) -> &'static str {
        "postgres"
    }

    fn extensions(&self) -> &[&str] {
        &["sql", "psql", "pgsql"]
    }

    fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_postgres::LANGUAGE.into())
            .map_err(|e| ExtractError::ParserInit {
                detail: strip_controls(&e.to_string()),
            })?;

        let src = content.as_bytes();
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| ExtractError::ParserInit {
                detail: format!("tree-sitter-postgres returned None for {file_path}"),
            })?;

        let root = tree.root_node();
        if has_error(root) {
            return Err(ExtractError::ParseContainsErrors {
                file_path: file_path.into(),
            });
        }

        let mut entities = Vec::new();
        let mut cursor = root.walk();
        for toplevel in root.named_children(&mut cursor) {
            // source_file → toplevel_stmt → stmt → <DDLNode>
            let mut c2 = toplevel.walk();
            for stmt in toplevel.named_children(&mut c2) {
                collect_stmt(stmt, src, file_path, &mut entities);
            }
        }

        Ok(entities)
    }
}
