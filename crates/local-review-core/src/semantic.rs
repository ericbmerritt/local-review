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
//! - [`plugins`] — `create_default_registry()` with all 13 languages

pub mod cache;
pub mod differ;
pub mod entity;
pub mod entity_id;
pub mod extractor;
pub mod identity;
pub mod plugins;
pub mod registry;

pub use differ::diff_entities;
pub use entity::{
    ChangeAnnotation, ChangeType, DescriptionSummary, EntityCoreData, EntityKind, EntitySummary,
    LineRange, RawEntity,
};
pub use entity_id::EntityId;
pub use extractor::{ExtractError, ExtractResult, SemanticExtractor};
pub use plugins::create_default_registry;
pub use registry::ExtractorRegistry;
