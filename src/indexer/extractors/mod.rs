pub mod text;

pub mod pdf;

use anyhow::Result;
use std::path::Path;

/// Supported file extensions for text extraction and watching.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["pdf", "txt", "md", "log"];

/// Single-method extractor — returns Ok(None) for unsupported files.
/// Send + Sync bounds allow one shared chain across parallel extraction
/// (rayon splits the closure across threads).
pub trait Extractor: Send + Sync {
    /// Attempt text extraction. Returns Ok(None) if this extractor
    /// cannot handle this file type (not an error — try next extractor).
    /// Returns Err only on genuine extraction failures.
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>>;
}

/// Extracted content from a file.
#[derive(Debug, Clone)]
pub struct ExtractedContent {
    pub text: String,
    #[allow(dead_code)]
    pub title: Option<String>,
    #[allow(dead_code)]
    pub page_count: Option<usize>,
}
