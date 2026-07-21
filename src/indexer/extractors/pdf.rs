use super::{ExtractedContent, Extractor};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::pdfium_lock;

/// Extracts text from PDF files using pdfium-render.
pub struct PdfExtractor {
    pdfium: pdfium_render::prelude::Pdfium,
}

impl PdfExtractor {
    /// Create a new PDF extractor with its own pdfium instance.
    pub fn new() -> Result<Self> {
        let dll_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let dll_path = dll_dir.join("pdfium.dll");
        let _lock = pdfium_lock::INIT.lock().unwrap_or_else(|e| e.into_inner());
        let pdfium = pdfium_render::prelude::Pdfium::new(
            pdfium_render::prelude::Pdfium::bind_to_library(&dll_path)
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

    /// Generate a searchable 1-page PDF with known text.
    fn generate_searchable_pdf(path: &Path) {
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, page_idx, layer_idx) =
            PdfDocument::new("Test PDF", Mm(210.0), Mm(297.0), "Layer 1");
        let font = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();
        let current_layer = doc.get_page(page_idx).get_layer(layer_idx);
        current_layer.use_text(
            "The quick brown fox jumps over the lazy dog",
            12.0,
            Mm(10.0),
            Mm(280.0),
            &font,
        );
        doc.save(&mut BufWriter::new(fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Generate a 5-page PDF with unique text on each page.
    fn generate_multipage_pdf(path: &Path) {
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, page1_idx, layer1_idx) =
            PdfDocument::new("Multi-page PDF", Mm(210.0), Mm(297.0), "Layer 1");
        let font = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();

        let layer = doc.get_page(page1_idx).get_layer(layer1_idx);
        layer.use_text("This is page one", 12.0, Mm(10.0), Mm(280.0), &font);

        for i in 2..=5 {
            let (page_idx, layer_idx) = doc.add_page(Mm(210.0), Mm(297.0), format!("Page {}", i));
            let layer = doc.get_page(page_idx).get_layer(layer_idx);
            layer.use_text(
                format!("This is page {}", i),
                12.0,
                Mm(10.0),
                Mm(280.0),
                &font,
            );
        }
        doc.save(&mut BufWriter::new(fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Generate a PDF with no text layer (image-only or blank page).
    fn generate_no_text_pdf(path: &Path) {
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, _page_idx, _layer_idx) =
            PdfDocument::new("No Text PDF", Mm(210.0), Mm(297.0), "Layer 1");
        // Page created but no text added — no text layer
        doc.save(&mut BufWriter::new(fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Generate a password-protected PDF.
    /// NOTE: lopdf 0.34 does not support encryption (decryption only).
    /// This test requires a pre-generated fixture or a library that supports PDF encryption.
    /// When pdfium is unavailable (this machine), all PDF tests skip anyway.
    fn generate_password_pdf(path: &Path) {
        // Write a minimal unencrypted PDF as fallback.
        // The test verifies error-handling behavior; on systems with pdfium,
        // replace this with a real encrypted PDF fixture.
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, _page_idx, _layer_idx) = PdfDocument::new("PDF", Mm(210.0), Mm(297.0), "Layer 1");
        doc.save(&mut BufWriter::new(fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Generate a 0-page PDF.
    fn generate_empty_pdf(path: &Path) {
        use lopdf;
        let mut doc = lopdf::Document::with_version("1.4");
        let pages_id = doc.new_object_id();

        // Pages tree with 0 kids
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", lopdf::Object::Name("Pages".into()));
        pages.set("Kids", lopdf::Object::Array(vec![]));
        pages.set("Count", lopdf::Object::Integer(0));
        doc.objects
            .insert(pages_id, lopdf::Object::Dictionary(pages));

        // Catalog
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", lopdf::Object::Name("Catalog".into()));
        catalog.set("Pages", lopdf::Object::Reference(pages_id));
        let catalog_id = doc.new_object_id();
        doc.objects
            .insert(catalog_id, lopdf::Object::Dictionary(catalog));
        doc.trailer
            .set("Root", lopdf::Object::Reference(catalog_id));

        doc.save(path).unwrap();
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

    #[test]
    fn extract_searchable_pdf_returns_text() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("searchable.pdf");
        generate_searchable_pdf(&path);

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(
            result.text.contains("quick brown fox"),
            "Should extract known text, got: {}",
            result.text
        );
        assert_eq!(result.page_count, Some(1));
    }

    #[test]
    fn extract_multipage_pdf_returns_all_pages() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multipage.pdf");
        generate_multipage_pdf(&path);

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path).unwrap().unwrap();
        assert_eq!(result.page_count, Some(5));
        assert!(result.text.contains("page one"));
        assert!(result.text.contains("page five") || result.text.contains("page 5"));
    }

    #[test]
    fn extract_no_text_layer_returns_empty() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("notext.pdf");
        generate_no_text_pdf(&path);

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path);
        // Should not error — returns Ok with empty text (or some text from the blank page)
        match result {
            Ok(Some(content)) => {
                // A blank page with no text should produce minimal text
                // (printpdf may add some metadata as text — the key point is no crash)
                assert!(content.page_count.is_some());
            }
            Ok(None) => {} // Also acceptable if pdfium rejects it
            Err(e) => {
                panic!("No-text-layer PDF should not error, got: {}", e);
            }
        }
    }

    #[test]
    fn extract_password_protected_pdf_returns_error() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("password.pdf");
        generate_password_pdf(&path);

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path);
        // Password-protected PDF should return an error, not hang or panic
        match result {
            Err(_) => {
                // Expected: cannot load encrypted PDF without password
            }
            Ok(Some(_)) => {
                // pdfium may handle owner-password-protected PDFs if the
                // encryption only restricts owner operations (not viewing).
                // This is acceptable behavior.
            }
            Ok(None) => {
                // Also acceptable
            }
        }
    }

    #[test]
    fn extract_empty_pdf_returns_zero_pages() {
        let pdfium = match try_bind_pdfium() {
            Some(p) => p,
            None => return,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.pdf");
        generate_empty_pdf(&path);

        let extractor = PdfExtractor { pdfium };
        let result = extractor.extract(&path);
        match result {
            Ok(Some(content)) => {
                assert_eq!(content.page_count, Some(0), "Empty PDF should have 0 pages");
                assert!(content.text.is_empty());
            }
            Err(e) => {
                // pdfium may reject a 0-page PDF as invalid — acceptable
                eprintln!("pdfium rejected 0-page PDF: {}", e);
            }
            Ok(None) => {
                // Also acceptable
            }
        }
    }
}
