//! Markdown semantic extractor backed by `tree-sitter-md`.
//!
//! Extracts ATX-headed sections as entities. Each `section` node bounded by an
//! `atx_heading` becomes one entity; sections nest, so the Container Rule
//! suppresses ancestor sections when only a descendant changed.
//!
//! Node structure (tree-sitter-md):
//!   `document → section → atx_heading → (atx_h1_marker | …) + inline`
//!   Child sections are direct children of their parent section.

#![cfg(feature = "lang-markdown")]

use tree_sitter::{Node, Parser};

use crate::semantic::entity::{EntityKind, RawEntity};
use crate::semantic::extractor::{
    body_hash, build_entity_id, content_hash, sig_hash, ExtractError, ExtractResult,
    SemanticExtractor,
};
use crate::util::strip_controls;

// ── Name extraction ────────────────────────────────────────────────────────────

/// Return the text of the first `inline` child of an `atx_heading` node.
fn heading_text<'t>(heading: Node<'t>, src: &'t [u8]) -> Option<String> {
    for i in 0..heading.child_count() {
        let Ok(i32) = u32::try_from(i) else { continue };
        let Some(child) = heading.child(i32) else {
            continue;
        };
        if child.kind() == "inline" {
            let raw = child.utf8_text(src).unwrap_or("").trim().to_owned();
            let cleaned = strip_controls(&raw);
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

// ── Section walker ─────────────────────────────────────────────────────────────

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

    fn visit_section(&mut self, node: Node<'t>, parent_scope: &str) {
        // Find the atx_heading child of this section to get the name.
        let heading_node = (0..node.child_count())
            .filter_map(|i| u32::try_from(i).ok().and_then(|i32| node.child(i32)))
            .find(|c| c.kind() == "atx_heading");
        let Some(heading) = heading_node else {
            // Fenced sections without a heading (rare) — recurse into children.
            self.visit_children(node, parent_scope);
            return;
        };

        let Some(name) = heading_text(heading, self.src) else {
            self.visit_children(node, parent_scope);
            return;
        };

        let scope = if parent_scope.is_empty() {
            name.clone()
        } else {
            format!("{parent_scope}::{name}")
        };

        let entity_id = build_entity_id(&self.file_path, &scope, 0);
        let content = node.utf8_text(self.src).unwrap_or("").to_owned();
        let start_line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
        let end_line = u32::try_from(node.end_position().row + 1).unwrap_or(start_line);
        let ch = content_hash(&content);
        let sh = sig_hash(&content);
        let bh = body_hash(&content);

        self.entities.push(RawEntity {
            id: entity_id,
            scope: scope.clone(),
            kind: EntityKind::Section,
            start_line,
            end_line,
            content,
            content_hash: ch,
            sig_hash: sh,
            body_hash: bh,
            file_path: self.file_path.clone(),
        });

        // Walk child sections with this section as the new scope.
        self.visit_children(node, &scope);
    }

    fn visit_children(&mut self, node: Node<'t>, scope: &str) {
        for i in 0..node.child_count() {
            let Ok(i32) = u32::try_from(i) else { continue };
            let Some(child) = node.child(i32) else {
                continue;
            };
            if child.kind() == "section" {
                self.visit_section(child, scope);
            }
        }
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

pub(crate) struct MarkdownPlugin;

impl SemanticExtractor for MarkdownPlugin {
    fn id(&self) -> &'static str {
        "markdown"
    }

    fn extensions(&self) -> &[&str] {
        &["md", "markdown"]
    }

    fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_md::LANGUAGE.into())
            .map_err(|e| ExtractError::ParserInit {
                detail: format!("tree-sitter-md init failed for {file_path}: {e}"),
            })?;
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| ExtractError::ParserInit {
                detail: format!("tree-sitter-md parse returned None for {file_path}"),
            })?;

        let mut collector = Collector::new(content.as_bytes(), file_path);
        let root = tree.root_node();
        // Walk top-level sections directly under the document root.
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "section" {
                collector.visit_section(child, "");
            }
        }

        assign_ordinals(&mut collector.entities);
        Ok(collector.entities)
    }
}
