use super::{ExtractedContent, Extractor, SUPPORTED_EXTENSIONS};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Extracts text from plain text files (.txt, .md, .log).
pub struct TextExtractor;

impl Extractor for TextExtractor {
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>> {
        // Determine if this is a text file we handle
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_text = SUPPORTED_EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(ext) && *e != "pdf");

        if !is_text {
            return Ok(None);
        }

        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to read metadata: {}", path.display()))?;

        let file_size = metadata.len();
        let max_bytes: usize = 10 * 1024 * 1024; // 10MB limit

        let content = if file_size > max_bytes as u64 {
            // Read only the first 10MB
            use std::io::Read;
            let mut file = fs::File::open(path)?;
            let mut buf = vec![0u8; max_bytes];
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            String::from_utf8_lossy(&buf).to_string()
        } else {
            match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => {
                    // Non-UTF-8: use lossy decode
                    let bytes = fs::read(path)?;
                    String::from_utf8_lossy(&bytes).to_string()
                }
            }
        };

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        Ok(Some(ExtractedContent {
            text: content,
            title: Some(file_name.to_string()),
            page_count: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_utf8_txt_returns_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "Hello, world!").unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert_eq!(result.text, "Hello, world!");
    }

    #[test]
    fn extract_markdown_returns_raw_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("readme.md");
        fs::write(&path, "# Heading\n\n**bold** text").unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(result.text.contains("# Heading"));
        assert!(result.text.contains("**bold**"));
    }

    #[test]
    fn extract_non_text_extension_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.pdf");
        fs::write(&path, "not text").unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn extract_empty_file_returns_empty_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.txt");
        fs::write(&path, "").unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(result.text.is_empty());
    }

    #[test]
    fn extract_log_returns_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        fs::write(&path, "[INFO] Application started\n[WARN] Disk space low\n").unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        assert!(result.text.contains("Application started"));
        assert!(result.text.contains("Disk space low"));
    }

    #[test]
    fn extract_non_utf8_lossy_decode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("latin1.txt");
        // Write Latin-1 encoded bytes that are NOT valid UTF-8
        // 0xE9 = é in Latin-1, but invalid standalone byte in UTF-8
        let bytes: Vec<u8> = vec![
            0x48, 0x65, 0x6C, 0x6C, 0x6F, 0x20, // "Hello "
            0xE9, // invalid UTF-8 byte (would be part of 3-byte sequence)
            0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, // " world"
        ];
        fs::write(&path, &bytes).unwrap();

        let extractor = TextExtractor;
        let result = extractor.extract(&path).unwrap().unwrap();
        // Should decode with lossy replacement — no crash, text is present
        assert!(
            !result.text.is_empty(),
            "Should produce some text via lossy decode"
        );
        assert!(
            result.text.contains("Hello"),
            "Should contain ASCII prefix, got: {}",
            result.text
        );
    }

    #[test]
    fn extract_missing_file_returns_error() {
        let path = std::path::Path::new("/nonexistent/path/that/cannot/exist/file.txt");

        let extractor = TextExtractor;
        let result = extractor.extract(path);
        assert!(result.is_err(), "Missing file should return an error");
    }
}
