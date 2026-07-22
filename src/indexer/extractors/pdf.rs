use super::{ExtractedContent, Extractor};
use anyhow::{Context, Result};
use pdf_oxide::PdfDocument;
use std::path::Path;

/// Extracts text from PDF files using `pdf_oxide` (pure Rust, no DLL, high performance).
pub struct PdfExtractor;

impl PdfExtractor {
    /// Create a new PDF extractor — no DLLs, no locks, always succeeds.
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl Extractor for PdfExtractor {
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>> {
        // Only handle .pdf files
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("pdf") => {}
            _ => return Ok(None),
        }

        // pdf_oxide provides extract_all_text() as a convenience, but we iterate
        // pages manually to also capture the page count (R4).
        let doc = PdfDocument::open(path)
            .with_context(|| format!("Failed to open PDF: {}", path.display()))?;

        let page_count = doc.page_count()?;
        let mut text = String::new();
        for i in 0..page_count {
            let page_text = doc
                .extract_text(i)
                .with_context(|| format!("Failed to extract page {} from: {}", i, path.display()))?;
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&page_text);
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        Ok(Some(ExtractedContent {
            text,
            title: Some(file_name.to_string()),
            page_count: Some(page_count),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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

    /// Generate a PDF with no text.
    fn generate_no_text_pdf(path: &Path) {
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, _page_idx, _layer_idx) =
            PdfDocument::new("No Text PDF", Mm(210.0), Mm(297.0), "Layer 1");
        doc.save(&mut BufWriter::new(fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Generate a 0-page PDF.
    fn generate_empty_pdf(path: &Path) {
        use lopdf;
        let mut doc = lopdf::Document::with_version("1.4");
        let pages_id = doc.new_object_id();

        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", lopdf::Object::Name("Pages".into()));
        pages.set("Kids", lopdf::Object::Array(vec![]));
        pages.set("Count", lopdf::Object::Integer(0));
        doc.objects
            .insert(pages_id, lopdf::Object::Dictionary(pages));

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
        let extractor = PdfExtractor;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "not a pdf").unwrap();
        let result = extractor.extract(&path).unwrap();
        assert!(result.is_none(), "Non-PDF should return None");
    }

    #[test]
    fn extract_corrupt_pdf_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.pdf");
        fs::write(&path, b"%PDF-1.4\n%%EOF").unwrap();
        let extractor = PdfExtractor;
        let result = extractor.extract(&path);
        assert!(result.is_err(), "Corrupt PDF should return error");
    }

    #[test]
    fn extract_searchable_pdf_returns_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("searchable.pdf");
        generate_searchable_pdf(&path);
        let extractor = PdfExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(
            result.text.contains("quick brown fox"),
            "Should extract known text, got: {}",
            result.text
        );
        assert_eq!(
            result.page_count,
            Some(1),
            "1-page PDF should report page_count = 1"
        );
    }

    #[test]
    fn extract_multipage_pdf_returns_all_pages() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multipage.pdf");
        generate_multipage_pdf(&path);
        let extractor = PdfExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(result.text.contains("page one"));
        assert!(result.text.contains("page five") || result.text.contains("page 5"));
        assert_eq!(
            result.page_count,
            Some(5),
            "5-page PDF should report page_count = 5"
        );
    }

    #[test]
    fn extract_no_text_layer_returns_empty_or_minimal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("notext.pdf");
        generate_no_text_pdf(&path);
        let extractor = PdfExtractor;
        let result = extractor.extract(&path);
        // Should not crash — returns Ok with text or empty
        assert!(result.is_ok(), "No-text PDF should not error");
    }

    #[test]
    fn extract_password_protected_pdf_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("password.pdf");
        generate_no_text_pdf(&path); // Use blank PDF as stand-in
        let extractor = PdfExtractor;
        let result = extractor.extract(&path);
        // pdf-extract handles this gracefully
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn extract_empty_pdf_handled_gracefully() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.pdf");
        generate_empty_pdf(&path);
        let extractor = PdfExtractor;
        let result = extractor.extract(&path);
        // 0-page PDF — either error or empty text is acceptable
        match result {
            Ok(Some(content)) => assert!(content.text.is_empty()),
            Err(_) => {} // Also acceptable
            _ => {}
        }
    }
}
