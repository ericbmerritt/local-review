//! Registry that maps file extensions to `SemanticExtractor` implementations.

use std::collections::HashMap;
use std::path::Path;

use crate::semantic::entity::RawEntity;
use crate::semantic::extractor::{ExtractError, ExtractResult, SemanticExtractor};

/// Resolves file paths to the appropriate extractor and drives extraction.
pub struct ExtractorRegistry {
    extractors: Vec<Box<dyn SemanticExtractor>>,
    by_extension: HashMap<String, usize>,
}

impl ExtractorRegistry {
    /// Create an empty registry; use `register` to add extractors.
    pub fn new() -> Self {
        Self {
            extractors: Vec::new(),
            by_extension: HashMap::new(),
        }
    }

    /// Register an extractor. Each extension is mapped to the most recently
    /// registered extractor for that extension.
    pub fn register(&mut self, extractor: Box<dyn SemanticExtractor>) {
        let idx = self.extractors.len();
        for &ext in extractor.extensions() {
            self.by_extension.insert(ext.to_owned(), idx);
        }
        self.extractors.push(extractor);
    }

    /// Return the extractor for `file_path`, or `None` if unsupported.
    pub fn get(&self, file_path: &str) -> Option<&dyn SemanticExtractor> {
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
