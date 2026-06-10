//! Generic tree-sitter code plugin, parameterised by `LanguageSpec`.
//!
//! Each supported code language is an instance of `CodePlugin` with a
//! language-specific `LanguageSpec` constant. The spec declares which AST node
//! kinds are "entities" and how to extract names from them.

#![cfg(any(
    feature = "lang-rust",
    feature = "lang-python",
    feature = "lang-go",
    feature = "lang-java",
    feature = "lang-javascript",
    feature = "lang-typescript",
    feature = "lang-scala",
    feature = "lang-kotlin",
    feature = "lang-bash",
    feature = "lang-yaml",
    feature = "lang-json",
    feature = "lang-toml",
))]

use tree_sitter::{Language, Node, Parser, Tree};

use crate::semantic::entity::{EntityKind, RawEntity};
use crate::semantic::extractor::{
    body_hash, build_id_str, content_hash, sig_hash, ExtractError, ExtractResult, SemanticExtractor,
};
use crate::util::strip_controls;

// ── Language spec ─────────────────────────────────────────────────────────────

/// Static configuration for one language.
pub(crate) struct LanguageSpec {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    /// Returns the tree-sitter Language for this grammar.
    pub language_fn: fn() -> Language,
    /// Node kinds that produce top-level entities.
    pub entity_kinds: &'static [(&'static str, EntityKind)],
    /// Node kinds whose children may also contain entities (container nodes).
    pub container_kinds: &'static [&'static str],
}

// ── Name extraction ───────────────────────────────────────────────────────────

/// Extract the name text from a node using its `name` named field, or by
/// scanning for the first `identifier` / `type_identifier` child.
fn node_name<'t>(node: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    if let Some(n) = node.child_by_field_name("name") {
        return n.utf8_text(src).ok();
    }
    for child in node.named_children(&mut node.walk()) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "property_identifier"
        ) {
            return child.utf8_text(src).ok();
        }
    }
    None
}

/// Extract the text of any named-field `name` from node, stripping controls.
fn entity_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    node_name(node, src)
        .map(strip_controls)
        .filter(|s| !s.is_empty())
}

// ── Traversal ────────────────────────────────────────────────────────────────

struct Collector<'t> {
    src: &'t [u8],
    spec: &'static LanguageSpec,
    file_path: String,
    entities: Vec<RawEntity>,
}

impl<'t> Collector<'t> {
    fn new(src: &'t [u8], spec: &'static LanguageSpec, file_path: &str) -> Self {
        Self {
            src,
            spec,
            file_path: file_path.to_owned(),
            entities: Vec::new(),
        }
    }

    fn kind_for(&self, node_kind: &str) -> Option<EntityKind> {
        self.spec
            .entity_kinds
            .iter()
            .find(|(k, _)| *k == node_kind)
            .map(|(_, ek)| *ek)
    }

    fn visit(&mut self, node: Node<'t>, parent_scope: &str) {
        let kind_str = node.kind();
        if let Some(ek) = self.kind_for(kind_str) {
            if let Some(name) = entity_name(node, self.src) {
                let scope = if parent_scope.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_scope}::{name}")
                };
                let id_str = build_id_str(&self.file_path, kind_str, &scope);
                let content = node.utf8_text(self.src).unwrap_or("").to_owned();
                let start_line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
                let end_line = u32::try_from(node.end_position().row + 1).unwrap_or(start_line);
                let ch = content_hash(&content);
                let sh = sig_hash(&content);
                let bh = body_hash(&content);
                self.entities.push(RawEntity {
                    id_str,
                    scope: scope.clone(),
                    kind: ek,
                    start_line,
                    end_line,
                    content,
                    content_hash: ch,
                    sig_hash: sh,
                    body_hash: bh,
                    file_path: self.file_path.clone(),
                });
                // Walk children inside a container with scope set.
                let is_container = self.spec.container_kinds.contains(&kind_str);
                let child_scope = if is_container {
                    scope
                } else {
                    parent_scope.to_owned()
                };
                self.visit_children(node, &child_scope);
                return;
            }
        }
        self.visit_children(node, parent_scope);
    }

    fn visit_children(&mut self, node: Node<'t>, scope: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child, scope);
        }
    }
}

// ── Error-node detection ──────────────────────────────────────────────────────

fn has_error_nodes(root: Node<'_>) -> bool {
    if root.is_error() || root.is_missing() {
        return true;
    }
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if has_error_nodes(child) {
            return true;
        }
    }
    false
}

// ── Plugin implementation ─────────────────────────────────────────────────────

pub(crate) struct CodePlugin {
    spec: &'static LanguageSpec,
}

impl CodePlugin {
    pub(crate) const fn new(spec: &'static LanguageSpec) -> Self {
        Self { spec }
    }

    fn parse(&self, content: &str) -> Option<Tree> {
        let mut parser = Parser::new();
        let lang = (self.spec.language_fn)();
        parser.set_language(&lang).ok()?;
        parser.parse(content, None)
    }
}

impl SemanticExtractor for CodePlugin {
    fn id(&self) -> &'static str {
        self.spec.id
    }

    fn extensions(&self) -> &[&str] {
        self.spec.extensions
    }

    fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let tree = self
            .parse(content)
            .ok_or_else(|| ExtractError::ParserInit {
                detail: format!("tree-sitter parse returned None for {file_path}"),
            })?;

        let root = tree.root_node();
        if has_error_nodes(root) {
            return Err(ExtractError::ParseContainsErrors {
                file_path: file_path.into(),
            });
        }

        // Use content.as_bytes() directly — no copy. The Tree doesn't own the
        // source bytes; the caller (this method) keeps them alive for the
        // duration of extraction via the `content` reference.
        let mut collector = Collector::new(content.as_bytes(), self.spec, file_path);
        collector.visit(root, "");
        Ok(collector.entities)
    }
}

// ── Language specs ────────────────────────────────────────────────────────────

#[cfg(feature = "lang-rust")]
pub(crate) static RUST_SPEC: LanguageSpec = LanguageSpec {
    id: "rust",
    extensions: &["rs"],
    language_fn: || tree_sitter_rust::LANGUAGE.into(),
    entity_kinds: &[
        ("function_item", EntityKind::Function),
        ("struct_item", EntityKind::Struct),
        ("enum_item", EntityKind::Enum),
        ("trait_item", EntityKind::Trait),
        ("impl_item", EntityKind::Class),
        ("mod_item", EntityKind::Module),
        ("type_item", EntityKind::Type),
        ("const_item", EntityKind::Constant),
        ("static_item", EntityKind::Constant),
    ],
    container_kinds: &["impl_item", "mod_item", "trait_item"],
};

#[cfg(feature = "lang-python")]
pub(crate) static PYTHON_SPEC: LanguageSpec = LanguageSpec {
    id: "python",
    extensions: &["py"],
    language_fn: || tree_sitter_python::LANGUAGE.into(),
    entity_kinds: &[
        ("function_definition", EntityKind::Function),
        ("class_definition", EntityKind::Class),
    ],
    container_kinds: &["class_definition"],
};

#[cfg(feature = "lang-go")]
pub(crate) static GO_SPEC: LanguageSpec = LanguageSpec {
    id: "go",
    extensions: &["go"],
    language_fn: || tree_sitter_go::LANGUAGE.into(),
    entity_kinds: &[
        ("function_declaration", EntityKind::Function),
        ("method_declaration", EntityKind::Method),
        ("type_declaration", EntityKind::Type),
    ],
    container_kinds: &[],
};

#[cfg(feature = "lang-java")]
pub(crate) static JAVA_SPEC: LanguageSpec = LanguageSpec {
    id: "java",
    extensions: &["java"],
    language_fn: || tree_sitter_java::LANGUAGE.into(),
    entity_kinds: &[
        ("class_declaration", EntityKind::Class),
        ("interface_declaration", EntityKind::Interface),
        ("enum_declaration", EntityKind::Enum),
        ("method_declaration", EntityKind::Method),
        ("constructor_declaration", EntityKind::Method),
    ],
    container_kinds: &[
        "class_declaration",
        "enum_declaration",
        "interface_declaration",
    ],
};

#[cfg(feature = "lang-javascript")]
pub(crate) static JAVASCRIPT_SPEC: LanguageSpec = LanguageSpec {
    id: "javascript",
    extensions: &["js", "jsx", "mjs", "cjs"],
    language_fn: || tree_sitter_javascript::LANGUAGE.into(),
    entity_kinds: &[
        ("function_declaration", EntityKind::Function),
        ("class_declaration", EntityKind::Class),
        ("method_definition", EntityKind::Method),
    ],
    container_kinds: &["class_declaration"],
};

#[cfg(feature = "lang-typescript")]
pub(crate) static TYPESCRIPT_SPEC: LanguageSpec = LanguageSpec {
    id: "typescript",
    extensions: &["ts"],
    language_fn: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    entity_kinds: &[
        ("function_declaration", EntityKind::Function),
        ("class_declaration", EntityKind::Class),
        ("interface_declaration", EntityKind::Interface),
        ("type_alias_declaration", EntityKind::Type),
        ("enum_declaration", EntityKind::Enum),
        ("method_definition", EntityKind::Method),
    ],
    container_kinds: &["class_declaration", "interface_declaration"],
};

#[cfg(feature = "lang-typescript")]
pub(crate) static TSX_SPEC: LanguageSpec = LanguageSpec {
    id: "tsx",
    extensions: &["tsx"],
    language_fn: || tree_sitter_typescript::LANGUAGE_TSX.into(),
    entity_kinds: TYPESCRIPT_SPEC.entity_kinds,
    container_kinds: TYPESCRIPT_SPEC.container_kinds,
};

#[cfg(feature = "lang-scala")]
pub(crate) static SCALA_SPEC: LanguageSpec = LanguageSpec {
    id: "scala",
    extensions: &["scala", "sc"],
    language_fn: || tree_sitter_scala::LANGUAGE.into(),
    entity_kinds: &[
        ("class_definition", EntityKind::Class),
        ("object_definition", EntityKind::Class),
        ("trait_definition", EntityKind::Trait),
        ("function_definition", EntityKind::Function),
        ("val_definition", EntityKind::Constant),
    ],
    container_kinds: &["class_definition", "object_definition", "trait_definition"],
};

#[cfg(feature = "lang-kotlin")]
pub(crate) static KOTLIN_SPEC: LanguageSpec = LanguageSpec {
    id: "kotlin",
    extensions: &["kt", "kts"],
    language_fn: || tree_sitter_kotlin_ng::LANGUAGE.into(),
    entity_kinds: &[
        ("function_declaration", EntityKind::Function),
        ("class_declaration", EntityKind::Class),
        ("object_declaration", EntityKind::Class),
        ("primary_constructor", EntityKind::Method),
    ],
    container_kinds: &["class_declaration", "object_declaration"],
};

#[cfg(feature = "lang-bash")]
pub(crate) static BASH_SPEC: LanguageSpec = LanguageSpec {
    id: "bash",
    extensions: &["sh", "bash"],
    language_fn: || tree_sitter_bash::LANGUAGE.into(),
    entity_kinds: &[("function_definition", EntityKind::Function)],
    container_kinds: &[],
};

#[cfg(feature = "lang-yaml")]
pub(crate) static YAML_SPEC: LanguageSpec = LanguageSpec {
    id: "yaml",
    extensions: &["yaml", "yml"],
    language_fn: || tree_sitter_yaml::LANGUAGE.into(),
    entity_kinds: &[("block_mapping_pair", EntityKind::ConfigProperty)],
    container_kinds: &[],
};

#[cfg(feature = "lang-json")]
pub(crate) static JSON_SPEC: LanguageSpec = LanguageSpec {
    id: "json",
    extensions: &["json"],
    language_fn: || tree_sitter_json::LANGUAGE.into(),
    entity_kinds: &[("pair", EntityKind::ConfigProperty)],
    container_kinds: &[],
};

#[cfg(feature = "lang-toml")]
pub(crate) static TOML_SPEC: LanguageSpec = LanguageSpec {
    id: "toml",
    extensions: &["toml"],
    language_fn: || tree_sitter_toml_ng::LANGUAGE.into(),
    entity_kinds: &[
        ("table", EntityKind::ConfigProperty),
        ("array_table", EntityKind::ConfigProperty),
        ("pair", EntityKind::ConfigProperty),
    ],
    container_kinds: &["table", "array_table"],
};
