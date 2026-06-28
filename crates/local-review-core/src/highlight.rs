//! Syntax highlighting for the diff view.
//!
//! `highlight_file` parses source content with tree-sitter and returns
//! per-line token colour spans. Context, added, and removed lines all
//! consume these: context lines get token foregrounds on the default
//! background, while added/removed lines pair the token foregrounds with a
//! green/red background tint (GitHub model).
//!
//! Colours follow GitHub's dark-mode syntax theme (exact RGB values from
//! GitHub's Primer design system). Theming support is intentionally deferred.

use ratatui::style::Color;

/// A coloured byte-offset span within one source line.
///
/// `start` and `end` are byte offsets into the raw `RenderedLine::text`,
/// **excluding** the two-cell diff prefix ("  ", "+ ", "- ").
/// Spans within a line are non-overlapping and sorted by `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub start: usize,
    pub end: usize,
    pub color: Color,
}

/// Return per-line syntax highlight spans for `content`.
///
/// Index `i` of the outer `Vec` holds the token spans for line `i`
/// (0-based). Returns an empty outer `Vec` when the file's language is
/// unsupported or parsing fails. Each inner `Vec` is in source order.
pub fn highlight_file(content: &str, file_path: &str) -> Vec<Vec<TokenSpan>> {
    impl_detail::run(content, file_path)
}

// ── feature-gated implementation ─────────────────────────────────────────────

#[cfg(any(
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
    feature = "lang-postgres",
    feature = "lang-markdown",
    feature = "lang-nix",
))]
mod impl_detail {
    use std::path::Path;

    use ratatui::style::Color;
    use tree_sitter::{Node, Parser};

    use super::TokenSpan;

    // GitHub dark-mode syntax palette — exact RGB values from Primer.
    const KEYWORD: Color = Color::Rgb(255, 123, 114); // #ff7b72
    const STRING: Color = Color::Rgb(165, 214, 255); // #a5d6ff
    const COMMENT: Color = Color::Rgb(139, 148, 158); // #8b949e
    const NUMBER: Color = Color::Rgb(121, 192, 255); // #79c0ff
    const TYPE_COLOR: Color = Color::Rgb(255, 166, 87); // #ffa657

    pub(super) fn run(content: &str, file_path: &str) -> Vec<Vec<TokenSpan>> {
        let Some(lang) = language_for(file_path) else {
            return Vec::new();
        };
        let mut parser = Parser::new();
        if parser.set_language(&lang).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(content, None) else {
            return Vec::new();
        };
        let lines: Vec<&str> = content.lines().collect();
        let starts = line_start_bytes(content);
        let mut per_line: Vec<Vec<TokenSpan>> = vec![Vec::new(); lines.len()];
        collect_tokens(tree.root_node(), &lines, &starts, &mut per_line);
        per_line
    }

    /// Byte offset of the first byte of each line (0-indexed).
    fn line_start_bytes(content: &str) -> Vec<usize> {
        let mut offsets = vec![0usize];
        for (byte_idx, c) in content.char_indices() {
            if c == '\n' {
                offsets.push(byte_idx + 1);
            }
        }
        offsets
    }

    fn collect_tokens(
        node: Node<'_>,
        lines: &[&str],
        line_start_bytes: &[usize],
        per_line: &mut [Vec<TokenSpan>],
    ) {
        // MISSING nodes are zero-width synthetic insertions; skip them.
        // ERROR nodes are not skipped — their leaf children (keywords, literals)
        // are still correctly identified by kind(), so we recurse into them.
        // This is critical for partial-file reconstructions (diff hunks) where
        // the top-level structure is syntactically invalid but the tokens within
        // each line are still accurate.
        if node.is_missing() {
            return;
        }
        if node.child_count() == 0 {
            if let Some(color) = token_color(node) {
                emit_token(node, color, lines, line_start_bytes, per_line);
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_tokens(child, lines, line_start_bytes, per_line);
        }
    }

    fn emit_token(
        node: Node<'_>,
        color: Color,
        lines: &[&str],
        line_start_bytes: &[usize],
        per_line: &mut [Vec<TokenSpan>],
    ) {
        let start_row = node.start_position().row;
        // Multi-line tokens (block comments, multi-line strings) are skipped
        // in v1 — we only colour single-line leaf nodes.
        if node.end_position().row != start_row {
            return;
        }
        let Some(&line_start) = line_start_bytes.get(start_row) else {
            return;
        };
        let Some(line_text) = lines.get(start_row) else {
            return;
        };
        let start = node
            .start_byte()
            .saturating_sub(line_start)
            .min(line_text.len());
        let end = node
            .end_byte()
            .saturating_sub(line_start)
            .min(line_text.len());
        if start >= end {
            return;
        }
        if let Some(spans) = per_line.get_mut(start_row) {
            spans.push(TokenSpan { start, end, color });
        }
    }

    fn token_color(node: Node<'_>) -> Option<Color> {
        let kind = node.kind();
        if !node.is_named() {
            // Anonymous nodes whose kind is all-alphabetic represent keywords
            // (e.g. `fn`, `let`, `if`, `class`, `def`) — not punctuation.
            if kind.len() > 1 && kind.chars().all(char::is_alphabetic) {
                return Some(KEYWORD);
            }
            return None;
        }
        if kind.contains("comment") {
            return Some(COMMENT);
        }
        if kind.contains("string") || kind == "char_literal" {
            return Some(STRING);
        }
        if kind.contains("number") || kind.contains("integer") || kind.contains("float") {
            return Some(NUMBER);
        }
        if matches!(
            kind,
            "true"
                | "false"
                | "null"
                | "nil"
                | "none"
                | "True"
                | "False"
                | "None"
                | "undefined"
                | "boolean_literal"
        ) {
            return Some(NUMBER);
        }
        if matches!(kind, "type_identifier" | "primitive_type" | "builtin_type") {
            return Some(TYPE_COLOR);
        }
        None
    }

    fn language_for(file_path: &str) -> Option<tree_sitter::Language> {
        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())?
            .to_lowercase();
        match ext.as_str() {
            #[cfg(feature = "lang-rust")]
            "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
            #[cfg(feature = "lang-python")]
            "py" => Some(tree_sitter_python::LANGUAGE.into()),
            #[cfg(feature = "lang-go")]
            "go" => Some(tree_sitter_go::LANGUAGE.into()),
            #[cfg(feature = "lang-java")]
            "java" => Some(tree_sitter_java::LANGUAGE.into()),
            #[cfg(feature = "lang-javascript")]
            "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
            #[cfg(feature = "lang-typescript")]
            "ts" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            #[cfg(feature = "lang-typescript")]
            "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            #[cfg(feature = "lang-scala")]
            "scala" => Some(tree_sitter_scala::LANGUAGE.into()),
            #[cfg(feature = "lang-kotlin")]
            "kt" | "kts" => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
            #[cfg(feature = "lang-bash")]
            "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
            #[cfg(feature = "lang-yaml")]
            "yaml" | "yml" => Some(tree_sitter_yaml::LANGUAGE.into()),
            #[cfg(feature = "lang-json")]
            "json" => Some(tree_sitter_json::LANGUAGE.into()),
            #[cfg(feature = "lang-toml")]
            "toml" => Some(tree_sitter_toml_ng::LANGUAGE.into()),
            #[cfg(feature = "lang-postgres")]
            "sql" | "pgsql" => Some(tree_sitter_postgres::LANGUAGE.into()),
            #[cfg(feature = "lang-markdown")]
            "md" | "markdown" => Some(tree_sitter_md::LANGUAGE.into()),
            #[cfg(feature = "lang-nix")]
            "nix" => Some(tree_sitter_nix::LANGUAGE.into()),
            _ => None,
        }
    }
}

#[cfg(not(any(
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
    feature = "lang-postgres",
    feature = "lang-markdown",
    feature = "lang-nix",
)))]
mod impl_detail {
    use super::TokenSpan;
    pub(super) fn run(_content: &str, _file_path: &str) -> Vec<Vec<TokenSpan>> {
        Vec::new()
    }
}
