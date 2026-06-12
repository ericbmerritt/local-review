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
/// History: 1 = original; 2 = added markdown + test-file plugins (Phase 3);
/// 3 = added `AnchorFingerprint` / entity-reviewed fields (Phase 4).
pub const SCHEMA_VERSION: u32 = 3;

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
    /// Forward-compatibility slot for the dependency graph (Phase 5).
    /// `None` until graph computation is implemented.
    pub graph: Option<GraphData>,
    /// Files for which extraction failed (for display as fallback rows).
    pub failed_files: Vec<String>,
}

/// Placeholder for the dependency graph (populated in Phase 5).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GraphData {
    // populated in Phase 5
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
            failed_files: Vec::new(),
        }
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
