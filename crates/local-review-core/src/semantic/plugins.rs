//! Language plugin registration.
//!
//! Absorbed from sem-core (<https://github.com/Ataraxy-Labs/sem>, MIT/Apache-2.0).
//! Commit pinned at absorption time — see NOTICE in crate root.

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
    feature = "lang-toml"
))]
mod code;

#[cfg(feature = "lang-postgres")]
mod sql;

#[cfg(feature = "lang-markdown")]
mod markdown;

#[cfg(feature = "lang-typescript")]
mod test;

use crate::semantic::registry::ExtractorRegistry;

/// Build a registry with all language plugins enabled at compile time.
///
/// Languages are gated by feature flags; the default feature set enables all
/// 13 supported languages.
pub fn create_default_registry() -> ExtractorRegistry {
    let mut r = ExtractorRegistry::new();

    // Test plugin registered first so filename patterns (.spec., .test.) take
    // priority over the extension-based TypeScript/JavaScript dispatchers.
    #[cfg(feature = "lang-typescript")]
    r.register(Box::new(test::TestPlugin));

    #[cfg(feature = "lang-rust")]
    r.register(Box::new(code::CodePlugin::new(&code::RUST_SPEC)));

    #[cfg(feature = "lang-python")]
    r.register(Box::new(code::CodePlugin::new(&code::PYTHON_SPEC)));

    #[cfg(feature = "lang-go")]
    r.register(Box::new(code::CodePlugin::new(&code::GO_SPEC)));

    #[cfg(feature = "lang-java")]
    r.register(Box::new(code::CodePlugin::new(&code::JAVA_SPEC)));

    #[cfg(feature = "lang-javascript")]
    r.register(Box::new(code::CodePlugin::new(&code::JAVASCRIPT_SPEC)));

    #[cfg(feature = "lang-typescript")]
    {
        r.register(Box::new(code::CodePlugin::new(&code::TYPESCRIPT_SPEC)));
        r.register(Box::new(code::CodePlugin::new(&code::TSX_SPEC)));
    }

    #[cfg(feature = "lang-scala")]
    r.register(Box::new(code::CodePlugin::new(&code::SCALA_SPEC)));

    #[cfg(feature = "lang-kotlin")]
    r.register(Box::new(code::CodePlugin::new(&code::KOTLIN_SPEC)));

    #[cfg(feature = "lang-bash")]
    r.register(Box::new(code::CodePlugin::new(&code::BASH_SPEC)));

    #[cfg(feature = "lang-yaml")]
    r.register(Box::new(code::CodePlugin::new(&code::YAML_SPEC)));

    #[cfg(feature = "lang-json")]
    r.register(Box::new(code::CodePlugin::new(&code::JSON_SPEC)));

    #[cfg(feature = "lang-toml")]
    r.register(Box::new(code::CodePlugin::new(&code::TOML_SPEC)));

    #[cfg(feature = "lang-postgres")]
    r.register(Box::new(sql::SqlPlugin));

    #[cfg(feature = "lang-markdown")]
    r.register(Box::new(markdown::MarkdownPlugin));

    r
}
