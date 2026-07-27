//! Per-comment Claude context bundle.
//!
//! Replaces jjr's raw-hunk Claude prompt with a structured bundle that gives
//! Claude the target entity's full body plus its direct callers and callees,
//! so it can address a comment without breaking adjacent code. The bundle
//! is rendered as Markdown matching the shape Claude parses most reliably:
//!
//! ````text
//! ## Comment (severity: required)
//!
//! <body>
//!
//! ## Target: `auth::AuthService::authenticate` at src/auth.rs lines 42-78
//!
//! <full entity body>
//!
//! ## Direct dependencies (entities called by target)
//!
//! ### `db::Session::parse` at src/db.rs lines 12-30
//!
//! <body>
//!
//! ## Direct dependents (entities that call target)
//!
//! ### `LoginHandler::run` at src/handlers/login.rs lines 8-22
//!
//! <body>
//!
//! ## Diff hunk at src/auth.rs line 56
//!
//! ```diff
//! <hunk lines>
//! ```
//!
//! context truncated to budget; 3 of 8 dependents omitted
//! ````
//!
//! This module owns the data shape and Markdown renderer. Budget-aware
//! truncation (`render_with_budget`) is layered on top: it drops dependents
//! first, then dependencies, then appends the truncation note so Claude
//! knows the picture is partial. Required sections (comment, target, hunk)
//! are always packed even if individually large — for the v1 contract,
//! exceeding the budget is preferable to dropping context Claude can't do
//! its job without.

use std::path::PathBuf;

use crate::severity::Severity;

/// Default per-comment Claude bundle budget, in tokens. Overridable via the
/// `JJR_CONTEXT_BUDGET` environment variable.
///
/// 16k is more generous than sem-core's 8k default — Claude's context
/// window is large and review benefits from richer entity context than the
/// CLI-agent case. If a typical review starts hitting truncation, raise
/// the env var rather than this constant; user-facing budget shouldn't
/// require a rebuild.
pub const DEFAULT_BUDGET_TOKENS: usize = 16_000;

/// Environment variable read by [`budget_from_env`].
pub const BUDGET_ENV_VAR: &str = "JJR_CONTEXT_BUDGET";

/// One entity in the bundle — the target itself, or one of its
/// dependencies / dependents. The `display_name` is the language-native
/// scope path the entity list shows (e.g., `auth::AuthService::authenticate`
/// for Rust, `AuthService.authenticate` for TypeScript). `body` is the full
/// source text of the entity at the after-state of the change.
#[derive(Debug, Clone)]
pub struct BundleEntity {
    pub display_name: String,
    pub file_path: PathBuf,
    pub line_range: (u32, u32),
    pub body: String,
}

/// All inputs needed to render a single per-comment Claude bundle.
///
/// `dependencies` and `dependents` are budget-bounded by the bundle's
/// renderer; the rest are required. Callers populate this struct fully
/// (with every known dep / dependent) and let `render_with_budget` decide
/// what fits — it's simpler to truncate at render time than to pre-trim
/// upstream where the budget isn't known.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub comment_body: String,
    pub comment_severity: Severity,
    pub target: BundleEntity,
    pub dependencies: Vec<BundleEntity>,
    pub dependents: Vec<BundleEntity>,
    /// File path of the diff hunk (typically the same file as `target`,
    /// but not always — comments scoped to a different file's hunk are
    /// possible during entity-aware re-anchoring).
    pub hunk_file: PathBuf,
    /// 1-based after-state line of the commented row.
    pub hunk_line: u32,
    /// The hunk text exactly as it appears in the diff, including the
    /// `@@ -N,N +N,N @@` header and the `+`/`-`/` ` line prefixes. We
    /// don't try to re-render it from a parsed `Hunk` — Claude needs to
    /// see the line markers, and round-tripping through structured types
    /// loses the formatting we want to preserve.
    pub hunk_text: String,
}

/// Render the bundle in full — no budget enforcement, no truncation note.
/// Useful in tests and as the building block for `render_with_budget`.
pub fn render(bundle: &Bundle) -> String {
    render_inner(
        bundle,
        bundle.dependencies.len(),
        bundle.dependents.len(),
        0,
        0,
    )
}

/// Render with truncation telemetry baked into the prompt. The two `omitted`
/// counts feed the "context truncated to budget; X of Y dependents omitted"
/// note Claude sees at the end of the prompt; callers compute them based on
/// how many items the budget allowed in. Pass zero for the omitted counts
/// to suppress the note.
pub fn render_with_truncation(
    bundle: &Bundle,
    deps_kept: usize,
    dependents_kept: usize,
    deps_total: usize,
    dependents_total: usize,
) -> String {
    let deps_omitted = deps_total.saturating_sub(deps_kept);
    let dependents_omitted = dependents_total.saturating_sub(dependents_kept);
    render_inner(
        bundle,
        deps_kept,
        dependents_kept,
        deps_omitted,
        dependents_omitted,
    )
}

fn render_inner(
    bundle: &Bundle,
    deps_kept: usize,
    dependents_kept: usize,
    deps_omitted: usize,
    dependents_omitted: usize,
) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(2048);

    // 1. Comment (required).
    let _ = writeln!(
        out,
        "## Comment (severity: {})\n\n{}",
        severity_label(bundle.comment_severity),
        bundle.comment_body.trim_end()
    );

    // 2. Target entity (required).
    out.push('\n');
    let _ = writeln!(
        out,
        "## Target: `{}` at {} lines {}-{}\n",
        bundle.target.display_name,
        bundle.target.file_path.display(),
        bundle.target.line_range.0,
        bundle.target.line_range.1,
    );
    out.push_str(bundle.target.body.trim_end());
    out.push('\n');

    // 3. Direct dependencies (budget-bounded; section omitted entirely if empty).
    if deps_kept > 0 {
        out.push_str("\n## Direct dependencies (entities called by target)\n");
        for dep in bundle.dependencies.iter().take(deps_kept) {
            out.push_str(&entity_section(dep));
        }
    }

    // 4. Direct dependents (budget-bounded; section omitted entirely if empty).
    if dependents_kept > 0 {
        out.push_str("\n## Direct dependents (entities that call target)\n");
        for dependent in bundle.dependents.iter().take(dependents_kept) {
            out.push_str(&entity_section(dependent));
        }
    }

    // 5. Diff hunk (required).
    let _ = writeln!(
        out,
        "\n## Diff hunk at {} line {}\n\n```diff",
        bundle.hunk_file.display(),
        bundle.hunk_line
    );
    out.push_str(bundle.hunk_text.trim_end_matches('\n'));
    out.push_str("\n```\n");

    // 6. Truncation note (only when something was dropped).
    if deps_omitted > 0 || dependents_omitted > 0 {
        out.push('\n');
        out.push_str(&truncation_note(
            deps_kept,
            deps_omitted,
            dependents_kept,
            dependents_omitted,
        ));
        out.push('\n');
    }

    out
}

/// Approximate the token count of `s` using the char-count / 4 heuristic.
///
/// Per the spec: "Fast, simple, accurate enough for budgeting. Within ~20%
/// of actual token count for English/code mix." Char count (not byte count)
/// keeps multi-byte UTF-8 from inflating the estimate. We don't round up
/// the partial-token tail — over-counting tokens would shrink the bundle
/// unnecessarily.
pub fn count_tokens(s: &str) -> usize {
    s.chars().count() / 4
}

/// Read the budget from the `JJR_CONTEXT_BUDGET` env var; fall back to
/// [`DEFAULT_BUDGET_TOKENS`] when unset or unparseable. A value of zero is
/// passed through (the caller can use that as a sentinel for "always
/// truncate to required only") — we only fall back when the var is missing
/// or non-numeric.
pub fn budget_from_env() -> usize {
    std::env::var(BUDGET_ENV_VAR)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
}

/// Render the bundle, truncating dependents-then-dependencies until the
/// result fits `budget_tokens`. Required sections (comment, target, hunk)
/// are always packed — when even the required content exceeds the budget,
/// the bundle is shipped over-budget rather than dropping items Claude
/// can't do its job without. A truncation note is appended whenever any
/// item was dropped so Claude knows the picture is partial.
pub fn render_with_budget(bundle: &Bundle, budget_tokens: usize) -> String {
    let deps_total = bundle.dependencies.len();
    let dependents_total = bundle.dependents.len();
    let mut deps_kept = deps_total;
    let mut dependents_kept = dependents_total;

    // Iterative drop: each render is cheap (string concat), and there are
    // never more than a few dozen optional items per bundle, so a tight
    // loop is simpler than precomputing per-item sizes. The loop is
    // guaranteed to terminate — every iteration either fits or strictly
    // reduces the kept count, and once both counts hit zero we ship.
    loop {
        let out = render_with_truncation(
            bundle,
            deps_kept,
            dependents_kept,
            deps_total,
            dependents_total,
        );
        if count_tokens(&out) <= budget_tokens {
            return out;
        }
        // Drop dependents first (lower information density per token in
        // most cases), then dependencies. Once both hit zero the required
        // content alone is over budget; ship it anyway.
        if dependents_kept > 0 {
            dependents_kept -= 1;
        } else if deps_kept > 0 {
            deps_kept -= 1;
        } else {
            return out;
        }
    }
}

fn entity_section(entity: &BundleEntity) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(entity.body.len() + 128);
    let _ = writeln!(
        s,
        "\n### `{}` at {} lines {}-{}\n",
        entity.display_name,
        entity.file_path.display(),
        entity.line_range.0,
        entity.line_range.1,
    );
    s.push_str(entity.body.trim_end());
    s.push('\n');
    s
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Required => "required",
        Severity::Suggestion => "suggestion",
        Severity::Note => "note",
    }
}

fn truncation_note(
    deps_kept: usize,
    deps_omitted: usize,
    dependents_kept: usize,
    dependents_omitted: usize,
) -> String {
    // Build a single line listing whichever categories were truncated.
    // Pluralisation is by count, so "1 dependent" / "3 dependents" both
    // read naturally.
    let mut parts: Vec<String> = Vec::new();
    if deps_omitted > 0 {
        let total = deps_kept + deps_omitted;
        parts.push(format!(
            "{deps_omitted} of {total} {} omitted",
            plural(total, "dependency", "dependencies"),
        ));
    }
    if dependents_omitted > 0 {
        let total = dependents_kept + dependents_omitted;
        parts.push(format!(
            "{dependents_omitted} of {total} {} omitted",
            plural(total, "dependent", "dependents"),
        ));
    }
    format!("context truncated to budget; {}", parts.join("; "))
}

fn plural(n: usize, singular: &'static str, multiple: &'static str) -> &'static str {
    if n == 1 {
        singular
    } else {
        multiple
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn bundle_entity(name: &str, path: &str, lo: u32, hi: u32, body: &str) -> BundleEntity {
        BundleEntity {
            display_name: name.to_owned(),
            file_path: PathBuf::from(path),
            line_range: (lo, hi),
            body: body.to_owned(),
        }
    }

    fn sample_bundle(deps: usize, dependents: usize) -> Bundle {
        Bundle {
            comment_body: "fix the off-by-one here".to_owned(),
            comment_severity: Severity::Required,
            target: bundle_entity(
                "auth::AuthService::authenticate",
                "src/auth.rs",
                42,
                78,
                "fn authenticate(&self, creds: &Creds) -> Result<User> {\n    // ...\n}",
            ),
            dependencies: (0..deps)
                .map(|i| {
                    bundle_entity(
                        &format!("db::dep_{i}"),
                        &format!("src/db_{i}.rs"),
                        10,
                        20,
                        &format!("fn dep_{i}() {{}}"),
                    )
                })
                .collect(),
            dependents: (0..dependents)
                .map(|i| {
                    bundle_entity(
                        &format!("handlers::user_{i}"),
                        &format!("src/handlers/user_{i}.rs"),
                        5,
                        15,
                        &format!("fn user_{i}() {{}}"),
                    )
                })
                .collect(),
            hunk_file: PathBuf::from("src/auth.rs"),
            hunk_line: 56,
            hunk_text: "@@ -54,3 +54,3 @@\n     let user = self.db.lookup(creds);\n-    if user.valid {\n+    if user.is_valid {".to_owned(),
        }
    }

    #[test]
    fn render_includes_all_required_sections() {
        let bundle = sample_bundle(0, 0);
        let out = render(&bundle);
        assert!(out.contains("## Comment (severity: required)"));
        assert!(out.contains("fix the off-by-one here"));
        assert!(
            out.contains("## Target: `auth::AuthService::authenticate` at src/auth.rs lines 42-78")
        );
        assert!(out.contains("fn authenticate"));
        assert!(out.contains("## Diff hunk at src/auth.rs line 56"));
        assert!(out.contains("```diff"));
        assert!(out.contains("@@ -54,3 +54,3 @@"));
    }

    #[test]
    fn render_omits_dep_sections_when_empty() {
        // No deps and no dependents → those headings are absent entirely,
        // not shown as empty sections. Empty section headers are misleading
        // — they imply "no deps/dependents exist" when in v1 it might just
        // mean the graph wasn't built yet.
        let bundle = sample_bundle(0, 0);
        let out = render(&bundle);
        assert!(!out.contains("## Direct dependencies"));
        assert!(!out.contains("## Direct dependents"));
    }

    #[test]
    fn render_includes_dependency_subsections() {
        let bundle = sample_bundle(2, 0);
        let out = render(&bundle);
        assert!(out.contains("## Direct dependencies (entities called by target)"));
        assert!(out.contains("### `db::dep_0` at src/db_0.rs lines 10-20"));
        assert!(out.contains("### `db::dep_1` at src/db_1.rs lines 10-20"));
    }

    #[test]
    fn render_includes_dependent_subsections() {
        let bundle = sample_bundle(0, 1);
        let out = render(&bundle);
        assert!(out.contains("## Direct dependents (entities that call target)"));
        assert!(out.contains("### `handlers::user_0` at src/handlers/user_0.rs lines 5-15"));
    }

    #[test]
    fn render_severity_label_matches_canonical_strings() {
        let mut b = sample_bundle(0, 0);
        b.comment_severity = Severity::Suggestion;
        assert!(render(&b).contains("severity: suggestion"));
        b.comment_severity = Severity::Note;
        assert!(render(&b).contains("severity: note"));
    }

    #[test]
    fn no_truncation_note_when_nothing_dropped() {
        let bundle = sample_bundle(2, 2);
        let out = render_with_truncation(&bundle, 2, 2, 2, 2);
        assert!(
            !out.contains("context truncated"),
            "nothing dropped → no truncation note; got {out}"
        );
    }

    #[test]
    fn truncation_note_reports_dependent_count() {
        // 4 dependents existed, only 1 kept → "3 of 4 dependents omitted".
        let bundle = sample_bundle(0, 4);
        let out = render_with_truncation(&bundle, 0, 1, 0, 4);
        assert!(
            out.contains("context truncated to budget; 3 of 4 dependents omitted"),
            "expected dependents-only note; got {out}"
        );
    }

    #[test]
    fn truncation_note_reports_both_categories() {
        let bundle = sample_bundle(5, 3);
        let out = render_with_truncation(&bundle, 2, 1, 5, 3);
        // Both deps and dependents truncated → semicolon-joined report.
        assert!(out.contains("3 of 5 dependencies omitted"));
        assert!(out.contains("2 of 3 dependents omitted"));
        assert!(out.contains("context truncated to budget;"));
    }

    #[test]
    fn truncation_note_singular_form_for_single_item() {
        let bundle = sample_bundle(0, 1);
        let out = render_with_truncation(&bundle, 0, 0, 0, 1);
        // Total is 1 → singular "dependent". "1 of 1 dependent omitted".
        assert!(
            out.contains("1 of 1 dependent omitted"),
            "expected singular form; got {out}"
        );
    }

    #[test]
    fn count_tokens_uses_char_count_over_four() {
        // The char/4 heuristic; round-down so we slightly under-estimate
        // tokens and slightly over-spend the budget. Better than the other
        // direction, which would truncate aggressively.
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("abcd"), 1);
        assert_eq!(count_tokens("abc"), 0);
        assert_eq!(count_tokens("abcdefgh"), 2);
        // Multi-byte chars count as one each — heuristic is by `chars()`,
        // not `len()`, so an emoji-heavy comment doesn't inflate the count.
        assert_eq!(count_tokens("αβγδ"), 1);
    }

    #[test]
    #[serial]
    fn budget_from_env_returns_default_when_unset() {
        // The env var name is exposed via BUDGET_ENV_VAR; remove it first
        // so the test is hermetic.
        std::env::remove_var(BUDGET_ENV_VAR);
        assert_eq!(budget_from_env(), DEFAULT_BUDGET_TOKENS);
    }

    #[test]
    #[serial]
    fn budget_from_env_parses_integer_value() {
        std::env::set_var(BUDGET_ENV_VAR, "8000");
        assert_eq!(budget_from_env(), 8000);
        std::env::remove_var(BUDGET_ENV_VAR);
    }

    #[test]
    #[serial]
    fn budget_from_env_falls_back_on_unparseable_value() {
        std::env::set_var(BUDGET_ENV_VAR, "not-a-number");
        assert_eq!(budget_from_env(), DEFAULT_BUDGET_TOKENS);
        std::env::remove_var(BUDGET_ENV_VAR);
    }

    #[test]
    fn render_with_budget_packs_everything_when_under_budget() {
        // Generous budget → no truncation, no note.
        let bundle = sample_bundle(3, 3);
        let out = render_with_budget(&bundle, 100_000);
        assert!(!out.contains("context truncated"));
        assert!(out.contains("db::dep_0"));
        assert!(out.contains("db::dep_2"));
        assert!(out.contains("handlers::user_0"));
        assert!(out.contains("handlers::user_2"));
    }

    #[test]
    fn render_with_budget_drops_dependents_before_dependencies() {
        // Tight budget that fits required + a couple of deps but no
        // dependents. The drop order is dependents-first per spec.
        let bundle = sample_bundle(2, 2);
        // Tune the budget so it can only hold required + some deps.
        let full = render(&bundle);
        let full_tokens = count_tokens(&full);
        let dep_section_tokens = count_tokens(&entity_section(&bundle.dependents[0]))
            + count_tokens(&entity_section(&bundle.dependents[1]));
        // Aim just below "full - dependents" so dependents are dropped but
        // dependencies stay.
        let budget = full_tokens - dep_section_tokens + 1;
        let out = render_with_budget(&bundle, budget);
        // Some kind of truncation should have happened — confirm dependents
        // dropped first by checking the note and absence of dependent names.
        assert!(
            out.contains("context truncated"),
            "expected truncation note"
        );
        assert!(
            !out.contains("handlers::user_"),
            "dependents must be dropped first; got {out}"
        );
        // At least one dependency should still be present.
        assert!(
            out.contains("db::dep_0"),
            "dependencies should not be dropped first; got {out}"
        );
    }

    #[test]
    fn render_with_budget_ships_required_over_budget() {
        // Budget so tight that even the required sections exceed it. The
        // contract is: ship anyway, don't drop required items. The
        // truncation note still fires because the optional items WERE
        // dropped (they had to be).
        let bundle = sample_bundle(2, 2);
        let out = render_with_budget(&bundle, 1);
        // Required sections still present.
        assert!(out.contains("## Comment"));
        assert!(out.contains("## Target:"));
        assert!(out.contains("## Diff hunk"));
        // Optional sections fully dropped.
        assert!(!out.contains("## Direct dependencies"));
        assert!(!out.contains("## Direct dependents"));
    }

    #[test]
    fn render_with_truncation_keeps_kept_subsections() {
        // 5 deps total, keep 2 → only the first 2 dep names appear; the
        // others are dropped from the body (not mentioned anywhere except
        // the count in the truncation note).
        let bundle = sample_bundle(5, 0);
        let out = render_with_truncation(&bundle, 2, 0, 5, 0);
        assert!(out.contains("db::dep_0"));
        assert!(out.contains("db::dep_1"));
        assert!(!out.contains("db::dep_2"), "dep_2 should be dropped");
        assert!(out.contains("3 of 5 dependencies omitted"));
    }
}
