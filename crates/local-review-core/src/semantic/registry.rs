//! Registry that maps file extensions and filename patterns to `SemanticExtractor`
//! implementations.

use std::collections::HashMap;
use std::path::Path;

use crate::semantic::entity::RawEntity;
use crate::semantic::extractor::{CallSite, ExtractError, ExtractResult, SemanticExtractor};

/// Resolves file paths to the appropriate extractor and drives extraction.
///
/// Dispatch order:
/// 1. Filename-pattern match: the first registered extractor whose
///    `filename_patterns()` contains a substring found in the file's base name.
/// 2. Extension match: the most recently registered extractor for the extension.
pub struct ExtractorRegistry {
    extractors: Vec<Box<dyn SemanticExtractor>>,
    by_extension: HashMap<String, usize>,
    /// `(pattern, extractor_idx)` in registration order — first match wins.
    by_filename_pattern: Vec<(String, usize)>,
}

impl ExtractorRegistry {
    /// Create an empty registry; use `register` to add extractors.
    pub fn new() -> Self {
        Self {
            extractors: Vec::new(),
            by_extension: HashMap::new(),
            by_filename_pattern: Vec::new(),
        }
    }

    /// Register an extractor.
    ///
    /// Filename patterns are checked before extensions; within each group the
    /// first-registered extractor wins for patterns, and the last-registered
    /// extractor wins for extensions.
    pub fn register(&mut self, extractor: Box<dyn SemanticExtractor>) {
        let idx = self.extractors.len();
        for &pat in extractor.filename_patterns() {
            self.by_filename_pattern.push((pat.to_owned(), idx));
        }
        for &ext in extractor.extensions() {
            self.by_extension.insert(ext.to_owned(), idx);
        }
        self.extractors.push(extractor);
    }

    /// Return the extractor for `file_path`, or `None` if unsupported.
    pub fn get(&self, file_path: &str) -> Option<&dyn SemanticExtractor> {
        let file_name = Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Filename-pattern dispatch takes priority over extension dispatch.
        for (pattern, idx) in &self.by_filename_pattern {
            if file_name.contains(pattern.as_str()) {
                if let Some(ext) = self.extractors.get(*idx) {
                    return Some(ext.as_ref());
                }
            }
        }

        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())?
            .to_lowercase();
        self.by_extension
            .get(&ext)
            .and_then(|&i| self.extractors.get(i))
            .map(Box::as_ref)
    }

    /// Extract entities from `content` using the extractor for `file_path`.
    ///
    /// Returns `Err(UnsupportedLanguage)` if no extractor handles this file.
    pub fn extract(&self, content: &str, file_path: &str) -> ExtractResult {
        let Some(ext) = self.get(file_path) else {
            return Err(ExtractError::UnsupportedLanguage {
                file_path: file_path.into(),
            });
        };
        ext.extract(content, file_path)
    }

    /// Extract function/method call sites from `content` using the extractor
    /// for `file_path`. Returns an empty `Vec` if no extractor handles the
    /// file — call extraction is best-effort and an unknown language is the
    /// same as "no calls" for graph purposes.
    pub fn extract_calls(&self, content: &str, file_path: &str) -> Vec<CallSite> {
        match self.get(file_path) {
            Some(ext) => ext.extract_calls(content, file_path),
            None => Vec::new(),
        }
    }

    /// Extract from both before and after content and return a combined list
    /// suitable for diff computation.
    pub fn extract_both(
        &self,
        before: &str,
        after: &str,
        file_path: &str,
    ) -> Result<(Vec<RawEntity>, Vec<RawEntity>), ExtractError> {
        let before_entities = self.extract(before, file_path)?;
        let after_entities = self.extract(after, file_path)?;
        Ok((before_entities, after_entities))
    }
}

impl Default for ExtractorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
