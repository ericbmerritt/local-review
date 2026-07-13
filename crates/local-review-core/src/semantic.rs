//! Semantic entity extraction layer.
//!
//! Absorbed from sem-core (<https://github.com/Ataraxy-Labs/sem>, MIT/Apache-2.0).
//! See the NOTICE file in the crate root for attribution.
//!
//! # Overview
//!
//! - [`entity`] — public types: `EntityCoreData`, `EntityId`, `EntityKind`,
//!   `ChangeType`, `ChangeAnnotation`, `EntitySummary`, `DescriptionSummary`,
//!   `LineRange`
//! - [`entity_id`] — `EntityId` structured tuple + ordinal computation
//! - [`extractor`] — `SemanticExtractor` trait and `ExtractError`
//! - [`registry`] — `ExtractorRegistry` + `ExtractorRegistry::extract`
//! - [`differ`] — `diff_entities`: takes before/after `RawEntity` lists and
//!   produces `EntityCoreData` with Container Rule applied
//! - [`identity`] — Jaccard-based entity matching (used internally by differ)
//! - [`risk`] — total risk-tier mapping over changed entities
//! - [`cluster`] — concern clustering (connected components + affinity)
//! - [`sort`] — entity-list order modes (risk / dependency / file)
//! - [`plugins`] — `create_default_registry()` with all 13 languages

pub mod cache;
pub mod cluster;
pub mod context;
pub mod differ;
pub mod entity;
pub mod entity_id;
pub mod extractor;
pub mod graph;
pub mod identity;
pub mod plugins;
pub mod registry;
pub mod risk;
pub mod sort;

pub use cache::{GraphData, GraphEdge, GraphNode, UnresolvedRef};
pub use cluster::{cluster_entities, Clustering, GroupSpan};
pub use context::{
    render as render_bundle, render_with_truncation as render_bundle_with_truncation, Bundle,
    BundleEntity,
};
pub use differ::diff_entities;
pub use entity::{
    body_peek_from, body_peek_from_body, entity_for_line, fallback_summary_for_file,
    ChangeAnnotation, ChangeType, DescriptionSummary, EntityCoreData, EntityKind, EntitySummary,
    LineRange, RawEntity, RefactorKind,
};
pub use entity_id::EntityId;
pub use extractor::{CallSite, ExtractError, ExtractResult, SemanticExtractor};
pub use graph::build_graph;
pub use plugins::create_default_registry;
pub use registry::ExtractorRegistry;
pub use risk::{compute_risk_tiers, risk_tier, RiskAssessment, RiskTier};
pub use sort::{sort_entities, topo_sort_entities, OrderMode};
