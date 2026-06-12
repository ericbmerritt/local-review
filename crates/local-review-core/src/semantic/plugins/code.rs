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
    body_hash, build_entity_id, content_hash, sig_hash, ExtractError, ExtractResult,
    SemanticExtractor,
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
    /// Node kinds that are entities only when at the file's top level — i.e.,
    /// not nested inside any container. Used for kinds whose body-scope
    /// occurrences are noise (e.g. bash `variable_assignment` inside a
    /// function), but whose file-scope occurrences are meaningful (e.g.
    /// configuration variables and case-dispatch branches at script root).
    pub top_level_only_kinds: &'static [&'static str],
}

// ── Name extraction ───────────────────────────────────────────────────────────

/// Extract the name text from a node.
///
/// Search order:
/// 1. `name` field (most code languages)
/// 2. `key` field (YAML `block_mapping_pair`, JSON `pair`, TOML `pair`)
/// 3. First `identifier`/`type_identifier`/`property_identifier` child
fn node_name<'t>(node: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    if let Some(n) = node.child_by_field_name("name") {
        return n.utf8_text(src).ok();
    }
    if let Some(k) = node.child_by_field_name("key") {
        return k.utf8_text(src).ok();
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
///
/// Special-cased for Rust `impl_item` nodes: tree-sitter exposes both a
/// `trait` field (for trait impls) and a `type` field (the implementing
/// type). The generic name lookup would return whichever identifier comes
/// first lexically — for `impl Deref for PlanId`, that's "Deref" — making
/// every `impl Deref for X` block in a file collapse to the same display
/// name. Build a composite "Trait for Type" name so each impl is distinct
/// in the entity list and its methods get unique scope chains.
fn entity_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    if node.kind() == "impl_item" {
        return impl_item_name(node, src);
    }
    if node.kind() == "case_item" {
        return case_item_name(node, src);
    }
    node_name(node, src)
        .map(strip_controls)
        .filter(|s| !s.is_empty())
}

/// Name a bash `case_item` by the text of its pattern (the first named child).
/// Patterns can be a `word`, `string`, `raw_string`, `concatenation`, or
/// alternation like `'foo'|"bar"`. We render whatever was written so users
/// see the same text in the entity list as they see in the source.
fn case_item_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let pattern = node.named_children(&mut cursor).next()?;
    pattern
        .utf8_text(src)
        .ok()
        .map(strip_controls)
        .filter(|s| !s.is_empty())
}

/// Compose a name for a Rust `impl_item` node:
/// - `impl Trait for Type` → `"Trait for Type"`
/// - `impl Type`           → `"Type"`
///
/// Falls back to the generic name extractor if neither field is present
/// (e.g., an in-progress parse where the impl block is incomplete).
fn impl_item_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    let ty = node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(src).ok())
        .map(strip_controls)
        .filter(|s| !s.is_empty());
    let tr = node
        .child_by_field_name("trait")
        .and_then(|n| n.utf8_text(src).ok())
        .map(strip_controls)
        .filter(|s| !s.is_empty());
    match (tr, ty) {
        (Some(tr), Some(ty)) => Some(format!("{tr} for {ty}")),
        (None, Some(ty)) => Some(ty),
        (Some(tr), None) => Some(tr),
        (None, None) => node_name(node, src)
            .map(strip_controls)
            .filter(|s| !s.is_empty()),
    }
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
        // Tree-sitter is fault-tolerant: it produces a tree even for code it
        // can't fully parse, marking unparseable spans with ERROR or MISSING
        // nodes. Don't extract from inside those — their structure isn't
        // meaningful — but keep walking the rest of the file. Older grammar
        // versions stumble on newer language features (let-else, async fn in
        // traits, etc.); rejecting the whole file would erase navigation for
        // every modern Rust codebase.
        if node.is_error() || node.is_missing() {
            return;
        }
        let kind_str = node.kind();
        if let Some(ek) = self.kind_for(kind_str) {
            let restricted = self.spec.top_level_only_kinds.contains(&kind_str);
            if restricted && !parent_scope.is_empty() {
                // Top-level-only kind nested inside a container: skip emission,
                // but keep descending — children may still contain entities.
                self.visit_children(node, parent_scope);
                return;
            }
            if let Some(name) = entity_name(node, self.src) {
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

        // Use content.as_bytes() directly — no copy. The Tree doesn't own the
        // source bytes; the caller (this method) keeps them alive for the
        // duration of extraction via the `content` reference.
        let mut collector = Collector::new(content.as_bytes(), self.spec, file_path);
        collector.visit(root, "");

        // Assign ordinals so entities sharing the same (scope_chain,
        // signature_key) get distinct identities based on source order.
        // This disambiguates e.g. struct Foo (ordinal 0) from impl Foo
        // (ordinal 1) within the same file.
        let keys: Vec<_> = collector
            .entities
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
        for (entity, ord) in collector.entities.iter_mut().zip(ordinals.iter()) {
            entity.id.ordinal = *ord;
        }

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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
};

#[cfg(feature = "lang-typescript")]
pub(crate) static TSX_SPEC: LanguageSpec = LanguageSpec {
    id: "tsx",
    extensions: &["tsx"],
    language_fn: || tree_sitter_typescript::LANGUAGE_TSX.into(),
    entity_kinds: TYPESCRIPT_SPEC.entity_kinds,
    container_kinds: TYPESCRIPT_SPEC.container_kinds,
    top_level_only_kinds: TYPESCRIPT_SPEC.top_level_only_kinds,
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
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
};

// Bash entity model:
// - `function_definition` is always an entity, at any depth, and acts as a
//   container so nested functions get scoped names ("outer::inner").
// - `case_item` and `variable_assignment` are entities only at the file's top
//   level. Inside a function they're implementation detail and would explode
//   the entity list with noise; at the file level they're meaningful (CLI
//   dispatch patterns, configuration / exports / readonly assignments).
#[cfg(feature = "lang-bash")]
pub(crate) static BASH_SPEC: LanguageSpec = LanguageSpec {
    id: "bash",
    extensions: &["sh", "bash"],
    language_fn: || tree_sitter_bash::LANGUAGE.into(),
    entity_kinds: &[
        ("function_definition", EntityKind::Function),
        ("case_item", EntityKind::Function),
        ("variable_assignment", EntityKind::Constant),
    ],
    container_kinds: &["function_definition"],
    top_level_only_kinds: &["case_item", "variable_assignment"],
};

#[cfg(feature = "lang-yaml")]
pub(crate) static YAML_SPEC: LanguageSpec = LanguageSpec {
    id: "yaml",
    extensions: &["yaml", "yml"],
    language_fn: || tree_sitter_yaml::LANGUAGE.into(),
    entity_kinds: &[("block_mapping_pair", EntityKind::ConfigProperty)],
    container_kinds: &[],
    top_level_only_kinds: &[],
};

#[cfg(feature = "lang-json")]
pub(crate) static JSON_SPEC: LanguageSpec = LanguageSpec {
    id: "json",
    extensions: &["json"],
    language_fn: || tree_sitter_json::LANGUAGE.into(),
    entity_kinds: &[("pair", EntityKind::ConfigProperty)],
    container_kinds: &[],
    top_level_only_kinds: &[],
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
    top_level_only_kinds: &[],
};
