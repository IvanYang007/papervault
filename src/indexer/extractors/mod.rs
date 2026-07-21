pub mod text;

pub mod pdf;

mod private {
    use std::path::Path;
    use anyhow::Result;

    /// Single-method extractor — returns Ok(None) for unsupported files.
    pub trait Extractor {
        /// Attempt text extraction. Returns Ok(None) if this extractor
        /// cannot handle this file type (not an error — try next extractor).
        /// Returns Err only on genuine extraction failures.
        fn extract(&self, path: &Path) -> Result<Option<super::ExtractedContent>>;
    }
}

pub use private::Extractor;

/// Extracted content from a file.
#[derive(Debug, Clone)]
pub struct ExtractedContent {
    pub text: String,
    pub title: Option<String>,
    pub page_count: Option<usize>,
}
