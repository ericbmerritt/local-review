//! Risk-tier classification for changed entities.
//!
//! `risk_tier` is a pure, **total** function: every entity — including
//! fallback rows — lands in exactly one tier. The mapping follows the
//! review-comprehension spec (specs/review-comprehension.md, "Risk tiers"):
//! ordering High-tier entities first is the cheapest measurable
//! comprehension win (Fregnan et al.: ordering is causal), so the tier
//! must never silently degrade — unknown fan-out resolves **upward**.
//!
//! Fan-out means different things per change type: for signature changes
//! it is the number of direct callers; for deletions it is the number of
//! *surviving references* — after-state references that still name the
//! deleted symbol (dangling references). Before-state callers may already
//! have been updated, so the before-state graph is the wrong instrument;
//! survivors come from the graph's unresolved-reference records. Those
//! records are deduplicated per `(caller, callee name)` and capped per
//! file (see `semantic/graph.rs`), so the count is a lower bound on raw
//! call sites — roughly "referencing callers", not occurrences. The tier
//! mapping only relies on the zero / nonzero distinction, which the
//! dedup and cap preserve.

use crate::semantic::cache::GraphData;
use crate::semantic::entity::{ChangeAnnotation, ChangeType, EntitySummary};

/// Review-priority tier. Variant order is load-bearing for `rank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskTier {
    High,
    Medium,
    Low,
}

impl RiskTier {
    /// Sort key: High sorts first in ascending order.
    pub fn rank(self) -> u8 {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }

    /// Lowercase tier name for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// A tier plus its one-clause justification ("sig change · 11 callers").
/// Every assignment is explainable in one clause — tiers are hints the
/// reviewer can interrogate, never a black-box score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskAssessment {
    pub tier: RiskTier,
    pub clause: String,
}

fn tier(tier: RiskTier, clause: impl Into<String>) -> RiskAssessment {
    RiskAssessment {
        tier,
        clause: clause.into(),
    }
}

/// Classify one entity. `fan_out` is caller count for added/modified/moved
/// entities and surviving-reference count for deleted entities; `None`
/// means the graph is unavailable and resolves the tier upward.
pub fn risk_tier(entity: &EntitySummary, fan_out: Option<usize>) -> RiskAssessment {
    // Fallback rows first: extraction failed, so nothing below can be
    // trusted for this row — it cannot be shown low-risk, and unknown
    // fan-out must not escalate a whole-file placeholder to High.
    if entity.fallback {
        return tier(RiskTier::Medium, "unclassified — extraction failed");
    }
    if !entity.structural_change {
        return tier(RiskTier::Low, "cosmetic");
    }
    if entity.is_behavior_preserving() {
        return tier(RiskTier::Low, "behavior-preserving refactor");
    }
    match entity.change {
        ChangeType::Added => tier(RiskTier::Medium, "new behavior"),
        // A non-identical move is a move plus edits — body-change risk.
        // Annotation is `None` for moved entities, so sig changes hiding
        // inside a move cannot be distinguished; Medium is the honest floor.
        ChangeType::Moved => tier(RiskTier::Medium, "moved with edits"),
        ChangeType::Deleted => match fan_out {
            None => tier(RiskTier::High, "unverified references"),
            Some(0) => tier(RiskTier::Medium, "no surviving references"),
            Some(n) => tier(RiskTier::High, count_clause(n, "surviving reference")),
        },
        ChangeType::Modified => match entity.annotation {
            ChangeAnnotation::SigChanged | ChangeAnnotation::SigAndBody => match fan_out {
                None => tier(RiskTier::High, "sig change · unverified callers"),
                Some(0) => tier(RiskTier::Medium, "sig change · no callers"),
                Some(n) => tier(
                    RiskTier::High,
                    format!("sig change · {}", count_clause(n, "caller")),
                ),
            },
            ChangeAnnotation::BodyOnly => tier(RiskTier::Medium, "body change"),
            // Real modified entities always carry a sig/body annotation;
            // `None` only reaches here through paths that lost it —
            // classify as unverifiable rather than guessing low.
            ChangeAnnotation::None => tier(RiskTier::Medium, "unclassified — extraction failed"),
        },
    }
}

fn count_clause(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Compute and store the tier for every entity, reading fan-out from
/// `graph`. `graph: None` means the graph is unavailable — every fan-out
/// is unknown and tiers are degraded (callers surface this to the user).
pub fn compute_risk_tiers(entities: &mut [EntitySummary], graph: Option<&GraphData>) {
    for e in entities.iter_mut() {
        let fan_out = graph.map(|g| match e.change {
            ChangeType::Deleted => g
                .unresolved
                .iter()
                .filter(|u| u.callee_name == e.id.name())
                .count(),
            ChangeType::Added | ChangeType::Modified | ChangeType::Moved => {
                g.edges.iter().filter(|edge| edge.to == e.id).count()
            }
        });
        e.risk = Some(risk_tier(e, fan_out));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::semantic::cache::{GraphEdge, UnresolvedRef};
    use crate::semantic::entity::{EntityKind, RefactorKind};
    use crate::semantic::entity_id::EntityId;

    fn eid(name: &str) -> EntityId {
        EntityId::new(PathBuf::from("a.rs"), vec![name.to_owned()], None, 0)
    }

    fn entity(change: ChangeType, annotation: ChangeAnnotation) -> EntitySummary {
        EntitySummary {
            id: eid("subject"),
            display_name: "subject".to_owned(),
            kind: EntityKind::Function,
            change,
            annotation,
            file_path: PathBuf::from("a.rs"),
            source_file: None,
            target_line: None,
            line_range: (1, 10),
            structural_change: true,
            content_hash: 1,
            refactor: None,
            comment_count: 0,
            reviewed: false,
            risk: None,
            fallback: false,
        }
    }

    fn assert_tier(e: &EntitySummary, fan_out: Option<usize>, want: RiskTier, clause: &str) {
        let got = risk_tier(e, fan_out);
        assert_eq!(got.tier, want, "clause: {}", got.clause);
        assert_eq!(got.clause, clause);
    }

    // ── Modified: sig changes across caller availability ─────────────────────

    #[test]
    fn modified_sig_change_with_callers_is_high() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::SigChanged);
        assert_tier(&e, Some(11), RiskTier::High, "sig change · 11 callers");
        assert_tier(&e, Some(1), RiskTier::High, "sig change · 1 caller");
    }

    #[test]
    fn modified_sig_and_body_counts_as_sig_change() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::SigAndBody);
        assert_tier(&e, Some(3), RiskTier::High, "sig change · 3 callers");
    }

    #[test]
    fn modified_sig_change_unknown_callers_resolves_upward() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::SigChanged);
        assert_tier(&e, None, RiskTier::High, "sig change · unverified callers");
    }

    #[test]
    fn modified_sig_change_zero_callers_is_medium() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::SigChanged);
        assert_tier(&e, Some(0), RiskTier::Medium, "sig change · no callers");
    }

    // ── Modified: body / unclassified ─────────────────────────────────────────

    #[test]
    fn modified_body_only_is_medium_regardless_of_callers() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::BodyOnly);
        assert_tier(&e, Some(50), RiskTier::Medium, "body change");
        assert_tier(&e, Some(0), RiskTier::Medium, "body change");
        assert_tier(&e, None, RiskTier::Medium, "body change");
    }

    #[test]
    fn modified_without_annotation_is_medium_unclassified() {
        let e = entity(ChangeType::Modified, ChangeAnnotation::None);
        assert_tier(
            &e,
            None,
            RiskTier::Medium,
            "unclassified — extraction failed",
        );
    }

    // ── Added ─────────────────────────────────────────────────────────────────

    #[test]
    fn added_non_refactor_is_medium_for_any_caller_availability() {
        let e = entity(ChangeType::Added, ChangeAnnotation::None);
        assert_tier(&e, None, RiskTier::Medium, "new behavior");
        assert_tier(&e, Some(0), RiskTier::Medium, "new behavior");
        assert_tier(&e, Some(4), RiskTier::Medium, "new behavior");
    }

    #[test]
    fn added_extracted_is_low() {
        let mut e = entity(ChangeType::Added, ChangeAnnotation::None);
        e.refactor = Some(RefactorKind::Extracted {
            from: eid("origin"),
        });
        assert_tier(&e, None, RiskTier::Low, "behavior-preserving refactor");
    }

    // ── Deleted across survivor availability ─────────────────────────────────

    #[test]
    fn deleted_with_survivors_is_high() {
        let e = entity(ChangeType::Deleted, ChangeAnnotation::None);
        assert_tier(&e, Some(2), RiskTier::High, "2 surviving references");
        assert_tier(&e, Some(1), RiskTier::High, "1 surviving reference");
    }

    #[test]
    fn deleted_unknown_survivors_resolves_upward() {
        let e = entity(ChangeType::Deleted, ChangeAnnotation::None);
        assert_tier(&e, None, RiskTier::High, "unverified references");
    }

    #[test]
    fn deleted_zero_survivors_is_medium() {
        let e = entity(ChangeType::Deleted, ChangeAnnotation::None);
        assert_tier(&e, Some(0), RiskTier::Medium, "no surviving references");
    }

    // ── Moved / refactors / cosmetic ──────────────────────────────────────────

    #[test]
    fn moved_identical_is_low_moved_with_edits_is_medium() {
        let mut e = entity(ChangeType::Moved, ChangeAnnotation::None);
        e.refactor = Some(RefactorKind::Moved { identical: true });
        assert_tier(&e, None, RiskTier::Low, "behavior-preserving refactor");
        e.refactor = Some(RefactorKind::Moved { identical: false });
        assert_tier(&e, None, RiskTier::Medium, "moved with edits");
        e.refactor = None;
        assert_tier(&e, Some(0), RiskTier::Medium, "moved with edits");
    }

    #[test]
    fn pure_rename_is_low_impure_rename_tiers_by_annotation() {
        let mut e = entity(ChangeType::Modified, ChangeAnnotation::SigChanged);
        e.refactor = Some(RefactorKind::Renamed {
            from: "old".to_owned(),
            pure: true,
        });
        assert_tier(&e, None, RiskTier::Low, "behavior-preserving refactor");
        e.refactor = Some(RefactorKind::Renamed {
            from: "old".to_owned(),
            pure: false,
        });
        assert_tier(&e, Some(2), RiskTier::High, "sig change · 2 callers");
    }

    #[test]
    fn cosmetic_is_low_for_every_change_type() {
        for change in [
            ChangeType::Added,
            ChangeType::Modified,
            ChangeType::Deleted,
            ChangeType::Moved,
        ] {
            let mut e = entity(change, ChangeAnnotation::None);
            e.structural_change = false;
            assert_tier(&e, None, RiskTier::Low, "cosmetic");
        }
    }

    // ── Fallback rows ─────────────────────────────────────────────────────────

    #[test]
    fn fallback_rows_are_medium_even_with_unknown_fan_out() {
        for change in [
            ChangeType::Added,
            ChangeType::Modified,
            ChangeType::Deleted,
            ChangeType::Moved,
        ] {
            let mut e = entity(change, ChangeAnnotation::None);
            e.fallback = true;
            // Unknown fan-out must NOT escalate a fallback row to High —
            // the spec pins fallback rows to Medium with no badge.
            assert_tier(
                &e,
                None,
                RiskTier::Medium,
                "unclassified — extraction failed",
            );
        }
    }

    // ── compute_risk_tiers wiring ─────────────────────────────────────────────

    #[test]
    fn compute_counts_callers_from_edges_and_survivors_from_unresolved() {
        let mut sig = entity(ChangeType::Modified, ChangeAnnotation::SigChanged);
        sig.id = eid("validate");
        let mut deleted = entity(ChangeType::Deleted, ChangeAnnotation::None);
        deleted.id = eid("legacy_fn");
        let graph = GraphData {
            nodes: Vec::new(),
            edges: vec![
                GraphEdge {
                    from: eid("caller_a"),
                    to: eid("validate"),
                    call_sites: vec![3],
                },
                GraphEdge {
                    from: eid("caller_b"),
                    to: eid("validate"),
                    call_sites: vec![9],
                },
            ],
            unresolved: vec![UnresolvedRef {
                callee_name: "legacy_fn".to_owned(),
                from: eid("survivor"),
                line: 7,
            }],
        };
        let mut entities = vec![sig, deleted];
        compute_risk_tiers(&mut entities, Some(&graph));
        let sig_risk = entities[0].risk.as_ref().expect("computed");
        assert_eq!(sig_risk.tier, RiskTier::High);
        assert_eq!(sig_risk.clause, "sig change · 2 callers");
        let del_risk = entities[1].risk.as_ref().expect("computed");
        assert_eq!(del_risk.tier, RiskTier::High);
        assert_eq!(del_risk.clause, "1 surviving reference");
    }

    #[test]
    fn compute_without_graph_marks_unknown_fan_out() {
        let mut entities = vec![entity(ChangeType::Modified, ChangeAnnotation::SigChanged)];
        compute_risk_tiers(&mut entities, None);
        let risk = entities[0].risk.as_ref().expect("computed");
        assert_eq!(risk.tier, RiskTier::High);
        assert_eq!(risk.clause, "sig change · unverified callers");
    }

    #[test]
    fn rank_orders_high_before_medium_before_low() {
        assert!(RiskTier::High.rank() < RiskTier::Medium.rank());
        assert!(RiskTier::Medium.rank() < RiskTier::Low.rank());
    }
}
