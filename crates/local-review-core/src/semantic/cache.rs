//! Disk cache for extracted entity data.
//!
//! Cache entries are stored as JSON files keyed by commit SHA (ggr) or
//! `(change_id, content_hash)` (jjr). The cache stores only `EntityCoreData`
//! (the semantic extraction core) — UI-derived fields are computed at render
//! time.

use std::fs;
use std::path::{Path, PathBuf};

use snafu::Snafu;

use crate::semantic::entity::EntityCoreData;

/// Current schema version. Bump only when `EntityCoreData` or `CacheEntry`
/// gain a structural change incompatible with earlier readers.
///
/// New language plugins do NOT require a schema bump — they are handled
/// automatically via `extractor_fingerprint` stored in each `CacheEntry`.
///
/// History: 1 = original; 2 = added markdown + test-file plugins;
/// 3 = added `AnchorFingerprint` / entity-reviewed fields;
/// 4 = populated `GraphData` with real `nodes`/`edges`;
/// 5 = `GraphEdge.call_sites` + unresolved-reference records;
/// 6 = ggr graphs now built at the PR head SHA — pre-6 ggr entries hold
///     graphs built from a default-branch clone (wrong state for risk
///     tiers), so they must not be read back as valid.
pub const SCHEMA_VERSION: u32 = 6;

#[derive(Debug, Snafu)]
pub enum CacheError {
    #[snafu(display("cache write failed for {}: {source}", path.display()))]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("cache read failed for {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("cache JSON error for {}: {source}", path.display()))]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// Build-time hash of all `src/semantic/*.rs` source files, set by `build.rs`.
///
/// Any change to extraction logic — plugins, differ, Container Rule, identity
/// matcher — produces a different hash at compile time. Cache entries whose
/// `extraction_hash` does not match this constant are treated as misses and
/// trigger re-extraction automatically, with no manual version bump required.
pub const EXTRACTION_HASH: &str = env!("SEMANTIC_EXTRACTION_HASH");

/// The persisted cache entry.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub schema_version: u32,
    /// Build-time hash of the extraction source code that produced this entry.
    /// A mismatch means the logic changed; treat as miss and re-extract.
    #[serde(default)]
    pub extraction_hash: String,
    pub entities: Vec<EntityCoreData>,
    /// Cross-file call graph over every entity in the repo. Used by jjr's
    /// Claude bundle and by both tools' entity-list topo sort and caller
    /// counts. `None` when graph construction was skipped or failed
    /// (best-effort: a missing graph degrades the bundle without blocking
    /// the reviewer). jjr always builds a graph from the local working copy;
    /// ggr builds one when `--no-graph` is not set and a `/tmp` clone is
    /// available.
    pub graph: Option<GraphData>,
    /// Why `graph` is `None`, when the writer knows ("jj file list returned
    /// no files"). Surfaced verbatim in the degraded-tiers notice so a
    /// broken graph pipeline is diagnosable at a glance instead of failing
    /// silently. Additive: `serde(default)` keeps pre-existing entries
    /// readable. Meaningless when `graph` is `Some`.
    #[serde(default)]
    pub graph_failure: Option<String>,
    /// Files for which extraction failed (for display as fallback rows).
    pub failed_files: Vec<String>,
}

/// Cross-file call graph. Direct callers of an entity are the `from` side of
/// edges where `to == entity.id`; direct callees are the `to` side of edges
/// where `from == entity.id`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Call references whose callee could not be resolved to a known entity
    /// at the graphed state — dangling calls. Risk tiers use these to count
    /// surviving references to Deleted entities (spec: after-state dangling
    /// calls are the actual breakage); blast-radius lists them.
    #[serde(default)]
    pub unresolved: Vec<UnresolvedRef>,
}

/// One call site naming a callee that no entity at the graphed state defines.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnresolvedRef {
    /// The called name, as written at the call site.
    pub callee_name: String,
    /// The entity containing the call site.
    pub from: crate::semantic::entity::EntityId,
    /// 1-based line of the call site in `from`'s file (after state).
    pub line: u32,
}

/// One entity in the graph. The graph indexes every entity in the repo, not
/// just the changed entities in the current cache entry — callers/callees
/// of the change may live outside the diff.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    pub id: crate::semantic::entity::EntityId,
    pub kind: crate::semantic::entity::EntityKind,
}

/// A directed `caller → callee` edge between two entities by `EntityId`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphEdge {
    /// The entity that contains the call site.
    pub from: crate::semantic::entity::EntityId,
    /// The entity being called.
    pub to: crate::semantic::entity::EntityId,
    /// 1-based line numbers of each call occurrence in `from`'s file (after
    /// state) — one entry per call, so five calls yield five entries. Feeds
    /// the blast-radius overlay (one row per occurrence).
    #[serde(default)]
    pub call_sites: Vec<u32>,
}

// ── Read / write ─────────────────────────────────────────────────────────────

/// Write `entry` to `path`, creating parent directories as needed.
pub fn write(path: &Path, entry: &CacheEntry) -> Result<(), CacheError> {
    let json = serde_json::to_string(entry).map_err(|source| CacheError::Json {
        path: path.to_owned(),
        source,
    })?;
    // Atomic write: temp-file + rename, so an interrupted write never leaves
    // a truncated JSON file that would keep failing to parse.
    crate::util::atomic_write_bytes(path, json.as_bytes()).map_err(|source| CacheError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Read a cache entry from `path`, validating both the schema version and the
/// build-time extraction hash.
///
/// Returns `None` when the file does not exist, the schema version does not
/// match, or the extraction hash differs from [`EXTRACTION_HASH`].
/// All cases are treated as cache misses, not errors.
pub fn read(path: &Path) -> Result<Option<CacheEntry>, CacheError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CacheError::Read {
                path: path.to_owned(),
                source,
            })
        }
    };
    let entry: CacheEntry = serde_json::from_slice(&bytes).map_err(|source| CacheError::Json {
        path: path.to_owned(),
        source,
    })?;
    if entry.schema_version != SCHEMA_VERSION {
        return Ok(None);
    }
    if entry.extraction_hash != EXTRACTION_HASH {
        return Ok(None);
    }
    Ok(Some(entry))
}

// ── Path helpers ─────────────────────────────────────────────────────────────

/// Cache path for a ggr commit.
///
/// `base` is `$XDG_DATA_HOME/ggr/cache/entities/<owner>/<repo>/<pr>/`.
pub fn ggr_cache_path(base: &Path, commit_sha: &str) -> PathBuf {
    base.join(format!("{commit_sha}.json"))
}

/// Cache path for a jjr change.
///
/// `base` is the per-repo entities directory, e.g.
/// `$XDG_DATA_HOME/jjr/repos/<repo_path>/entities/`
/// (via `crate::store::repo_data_dir`). Keyed by `(change_id, commit_id)`
/// so that amending the change (new `commit_id`) produces a cache miss.
/// `commit_id` is formatted as-is (hex string from jj).
pub fn jjr_cache_path(base: &Path, change_id: &str, commit_id: &str) -> PathBuf {
    base.join(format!("{change_id}-{commit_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(schema_version: u32, hash: &str) -> CacheEntry {
        CacheEntry {
            schema_version,
            extraction_hash: hash.to_owned(),
            entities: Vec::new(),
            graph: None,
            graph_failure: None,
            failed_files: Vec::new(),
        }
    }

    #[test]
    fn graph_failure_round_trips_and_defaults_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("entry.json");
        let mut entry = make_entry(SCHEMA_VERSION, EXTRACTION_HASH);
        entry.graph_failure = Some("jj file list returned no files".to_owned());
        write(&path, &entry).expect("write");
        let read_back = read(&path).expect("read").expect("hit");
        assert_eq!(
            read_back.graph_failure.as_deref(),
            Some("jj file list returned no files")
        );

        // Entries written before the field existed must still deserialize.
        let legacy = format!(
            "{{\"schema_version\":{SCHEMA_VERSION},\"extraction_hash\":\"{EXTRACTION_HASH}\",\
             \"entities\":[],\"graph\":null,\"failed_files\":[]}}"
        );
        fs::write(&path, legacy).expect("write legacy");
        let read_back = read(&path).expect("read").expect("hit");
        assert_eq!(read_back.graph_failure, None);
    }

    #[test]
    fn roundtrip_empty_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        let entry = make_entry(SCHEMA_VERSION, EXTRACTION_HASH);
        write(&path, &entry).unwrap();
        let loaded = read(&path).unwrap().expect("entry must load");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert!(loaded.entities.is_empty());
    }

    #[test]
    fn schema_version_mismatch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json");
        write(&path, &make_entry(0, EXTRACTION_HASH)).unwrap();
        let result = read(&path).unwrap();
        assert!(result.is_none(), "mismatched schema version must be a miss");
    }

    #[test]
    fn extraction_hash_mismatch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old_logic.json");
        write(&path, &make_entry(SCHEMA_VERSION, "aabbccdd")).unwrap();
        let result = read(&path).unwrap();
        assert!(
            result.is_none(),
            "stale extraction logic must be a cache miss"
        );
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let result = read(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn jjr_cache_path_uses_commit_id() {
        let base = Path::new("/tmp/entities");
        let path = jjr_cache_path(base, "abc123", "deadbeef01234567");
        assert!(path
            .to_string_lossy()
            .ends_with("abc123-deadbeef01234567.json"));
    }

    #[test]
    fn ggr_cache_path_uses_sha() {
        let base = Path::new("/tmp/entities");
        let path = ggr_cache_path(base, "a1b2c3d4");
        assert_eq!(path, Path::new("/tmp/entities/a1b2c3d4.json"));
    }
}
