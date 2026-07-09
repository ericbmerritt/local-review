//! Jaccard-similarity entity matching across before/after extraction results.
//!
//! Adapted from sem-core's `model/identity.rs`. Matching proceeds in phases
//! from cheapest to most expensive.

use std::collections::{HashMap, HashSet};

use crate::semantic::entity::{ChangeAnnotation, EntityId, RawEntity};

/// Entities below this token count are not eligible for cross-file Jaccard
/// matching (trivial getter / stub noise reduction).
const MIN_TOKENS_CROSS_FILE: usize = 20;

/// Jaccard threshold for the fuzzy-match phase (Phase 5 in sem-core).
const FUZZY_THRESHOLD: f64 = 0.8;

/// Size-ratio pre-filter: skip if smaller/larger < this value.
const SIZE_RATIO_CUTOFF: f64 = 0.5;

// ── Token helpers ────────────────────────────────────────────────────────────

struct Tokens<'a> {
    count: usize,
    unique: HashSet<&'a str>,
}

fn tokenise(s: &str) -> Tokens<'_> {
    let mut count = 0usize;
    let mut unique = HashSet::new();
    for tok in s.split_whitespace() {
        count += 1;
        unique.insert(tok);
    }
    Tokens { count, unique }
}

fn to_f64(n: usize) -> f64 {
    #[expect(
        clippy::as_conversions,
        reason = "usize to f64 for token-count ratio; values are bounded by source file size"
    )]
    {
        n as f64
    }
}

fn jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        to_f64(intersection) / to_f64(union)
    }
}

fn similarity(a: &Tokens<'_>, b: &Tokens<'_>) -> f64 {
    let (small, large) = if a.count < b.count {
        (a.count, b.count)
    } else {
        (b.count, a.count)
    };
    if large > 0 && (to_f64(small) / to_f64(large)) < SIZE_RATIO_CUTOFF {
        return 0.0;
    }
    jaccard(&a.unique, &b.unique)
}

// ── Match phases ─────────────────────────────────────────────────────────────

fn id_match<'a>(
    before: &'a [RawEntity],
    after: &'a [RawEntity],
    mb: &mut HashSet<usize>,
    ma: &mut HashSet<usize>,
    out: &mut Vec<(&'a RawEntity, &'a RawEntity)>,
) {
    let by_id: HashMap<&EntityId, usize> =
        before.iter().enumerate().map(|(i, e)| (&e.id, i)).collect();
    for (ai, ae) in after.iter().enumerate() {
        if ma.contains(&ai) {
            continue;
        }
        if let Some(&bi) = by_id.get(&ae.id) {
            if !mb.contains(&bi) {
                mb.insert(bi);
                ma.insert(ai);
                out.push((&before[bi], ae));
            }
        }
    }
}

fn hash_match<'a>(
    before: &'a [RawEntity],
    after: &'a [RawEntity],
    mb: &mut HashSet<usize>,
    ma: &mut HashSet<usize>,
    out: &mut Vec<(&'a RawEntity, &'a RawEntity)>,
) {
    let mut by_hash: HashMap<u64, usize> = before
        .iter()
        .enumerate()
        .map(|(i, e)| (e.content_hash, i))
        .collect();
    for (ai, ae) in after.iter().enumerate() {
        if ma.contains(&ai) {
            continue;
        }
        if let Some(bi) = by_hash.remove(&ae.content_hash) {
            if !mb.contains(&bi) {
                mb.insert(bi);
                ma.insert(ai);
                out.push((&before[bi], ae));
            }
        }
    }
}

fn fuzzy_match<'a>(
    before: &'a [RawEntity],
    after: &'a [RawEntity],
    mb: &mut HashSet<usize>,
    ma: &mut HashSet<usize>,
    out: &mut Vec<(&'a RawEntity, &'a RawEntity)>,
) {
    let atoks: Vec<Tokens<'_>> = after.iter().map(|e| tokenise(&e.content)).collect();
    for (bi, be) in before.iter().enumerate() {
        if mb.contains(&bi) {
            continue;
        }
        let bt = tokenise(&be.content);
        if bt.count < MIN_TOKENS_CROSS_FILE {
            continue;
        }
        let mut best = FUZZY_THRESHOLD;
        let mut best_ai: Option<usize> = None;
        for (ai, ae) in after.iter().enumerate() {
            if ma.contains(&ai) || ae.kind != be.kind {
                continue;
            }
            let score = similarity(&bt, &atoks[ai]);
            if score > best {
                best = score;
                best_ai = Some(ai);
            }
        }
        if let Some(ai) = best_ai {
            mb.insert(bi);
            ma.insert(ai);
            out.push((be, &after[ai]));
        }
    }
}

// ── Extract-method detection ─────────────────────────────────────────────────

/// Containment threshold for extract detection: the fraction of an added
/// entity's unique tokens that must appear in a shrunken sibling's removed
/// token set. Tuned by the fixture tests in `differ.rs` (true-positive
/// extract vs. coincidental similarity): declaration boilerplate (fn, name,
/// braces) dilutes real extracts to ~0.65-0.75, while unrelated additions
/// score well under 0.2 — 0.6 splits the distributions with margin.
pub(crate) const EXTRACT_CONTAINMENT_THRESHOLD: f64 = 0.6;

/// Added entities below this token count are not eligible for extract
/// detection (trivial stubs match anything).
pub(crate) const MIN_TOKENS_EXTRACT: usize = 12;

/// For an Added entity, find a Modified sibling in the same before-file whose
/// removed tokens largely contain the added entity's tokens — the
/// extract-method shape. Returns the sibling's before-state id.
pub(crate) fn find_extraction_source(
    added: &RawEntity,
    matched: &[(&RawEntity, &RawEntity)],
) -> Option<EntityId> {
    let at = tokenise(&added.content);
    if at.count < MIN_TOKENS_EXTRACT {
        return None;
    }
    let mut best: Option<(f64, EntityId)> = None;
    for (be, ae) in matched {
        if be.file_path != added.file_path {
            continue;
        }
        let bt = tokenise(&be.content);
        let after_toks = tokenise(&ae.content);
        // The sibling must have shrunk in *tokens* — the heuristic is
        // token-based, and byte length shifts with formatting alone.
        if bt.count <= after_toks.count {
            continue;
        }
        // Removed span approximation: tokens present before but gone after.
        let removed: HashSet<&str> = bt.unique.difference(&after_toks.unique).copied().collect();
        if removed.is_empty() {
            continue;
        }
        let contained = at.unique.intersection(&removed).count();
        let containment = to_f64(contained) / to_f64(at.unique.len());
        if containment >= EXTRACT_CONTAINMENT_THRESHOLD
            && best.as_ref().is_none_or(|(b, _)| containment > *b)
        {
            best = Some((containment, be.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

// ── Annotation classification ────────────────────────────────────────────────

pub(crate) fn annotation(be: &RawEntity, ae: &RawEntity) -> ChangeAnnotation {
    let sig_changed = be.sig_hash != ae.sig_hash;
    // Compare body_hash (not content_hash) so that a signature-only change
    // doesn't incorrectly set body_changed = true. content_hash covers the
    // whole entity including the signature, so any sig change also changes
    // content_hash, making SigChanged unreachable with that approach.
    let body_changed = be.body_hash != ae.body_hash;
    match (sig_changed, body_changed) {
        (true, true) => ChangeAnnotation::SigAndBody,
        (true, false) => ChangeAnnotation::SigChanged,
        (false, _) => ChangeAnnotation::BodyOnly,
    }
}

pub(crate) fn is_structural_change(be: &RawEntity, ae: &RawEntity) -> bool {
    be.sig_hash != ae.sig_hash || has_non_comment_change(&be.content, &ae.content)
}

fn has_non_comment_change(before: &str, after: &str) -> bool {
    strip_comments(before) != strip_comments(after)
}

fn strip_comments(s: &str) -> String {
    s.lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("//") && !t.starts_with('#') && !t.starts_with("/*")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Public match entry point ─────────────────────────────────────────────────

pub(crate) struct MatchResult<'a> {
    pub matched: Vec<(&'a RawEntity, &'a RawEntity)>,
    pub added: Vec<&'a RawEntity>,
    pub deleted: Vec<&'a RawEntity>,
}

pub(crate) fn match_entities<'a>(
    before: &'a [RawEntity],
    after: &'a [RawEntity],
) -> MatchResult<'a> {
    let mut mb = HashSet::new();
    let mut ma = HashSet::new();
    let mut matched = Vec::new();

    id_match(before, after, &mut mb, &mut ma, &mut matched);
    hash_match(before, after, &mut mb, &mut ma, &mut matched);
    fuzzy_match(before, after, &mut mb, &mut ma, &mut matched);

    let added = after
        .iter()
        .enumerate()
        .filter_map(|(i, e)| if ma.contains(&i) { None } else { Some(e) })
        .collect();
    let deleted = before
        .iter()
        .enumerate()
        .filter_map(|(i, e)| if mb.contains(&i) { None } else { Some(e) })
        .collect();

    MatchResult {
        matched,
        added,
        deleted,
    }
}
