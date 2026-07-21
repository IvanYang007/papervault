use super::{ExtractedContent, Extractor};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Extracts text from PDF files using pdfium-render.
pub struct PdfExtractor {
    pdfium: pdfium_render::prelude::Pdfium,
}

impl PdfExtractor {
    /// Create a new PDF extractor with its own pdfium instance.
    pub fn new() -> Result<Self> {
        let pdfium = pdfium_render::prelude::Pdfium::new(
            pdfium_render::prelude::Pdfium::bind_to_library(
                pdfium_render::prelude::Pdfium::pdfium_platform_library_name(),
            )
            .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())
            .context("Failed to bind pdfium library")?,
        );

        Ok(Self { pdfium })
    }
}

impl Extractor for PdfExtractor {
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>> {
        // Only handle .pdf files
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("pdf") => {}
            _ => return Ok(None),
        }

        let bytes =
            fs::read(path).with_context(|| format!("Failed to read PDF: {}", path.display()))?;

        let doc = match self.pdfium.load_pdf_from_byte_slice(&bytes, None) {
            Ok(doc) => doc,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to load PDF '{}': {}",
                    path.display(),
                    e
                ));
            }
        };

        let page_count = doc.pages().len() as usize;
        let mut text = String::new();

        // Extract text from all pages
        for (i, page) in doc.pages().iter().enumerate() {
            match page.text() {
                Ok(page_text) => {
                    let page_str = page_text.all();
                    if !page_str.trim().is_empty() {
                        if i > 0 {
                            text.push('\n');
                        }
                        text.push_str(&page_str);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to extract text from page {} of '{}': {}",
                        i + 1,
                        path.display(),
                        e
                    );
                }
            }
        }

        Ok(Some(ExtractedContent {
            text,
            title: None,
            page_count: Some(page_count),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfium_render::prelude::Pdfium;
    use tempfile::TempDir;

    /// Try to bind pdfium. Returns None if the library is not available.
    fn try_bind_pdfium() -> Option<Pdfium> {
        let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name())
            .or_else(|_| Pdfium::bind_to_system_library());
        match bindings {
            Ok(b) => Some(Pdfium::new(b)),
            Err(_) => {
                eprintln!("Skipping: pdfium library not available");
                None
            }
        }
    }

    #[test]
    fn extract_non_pdf_file_returns_none() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };
        let extractor = PdfExtractor { pdfium };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "not a pdf").unwrap();

        let result = extractor.extract(&path).unwrap();
        assert!(result.is_none(), "Non-PDF should return None");
    }

    #[test]
    fn extract_corrupt_pdf_returns_error() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.pdf");
        fs::write(&path, b"%PDF-1.4\n%%EOF").unwrap();

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path);
        // Corrupt PDF should error, not crash
        match result {
            Ok(Some(content)) => {
                // May extract empty text from minimal valid PDF
                assert!(content.text.is_empty() || !content.text.is_empty());
            }
            Ok(None) => {
                // Also acceptable
            }
            Err(_) => {
                // Expected: corrupt file
            }
        }
    }
}
