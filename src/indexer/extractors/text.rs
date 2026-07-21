use super::{ExtractedContent, Extractor};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Extracts text from plain text files (.txt, .md, .log).
pub struct TextExtractor;

impl Extractor for TextExtractor {
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>> {
        // Determine if this is a text file we handle
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_text = matches!(ext.to_lowercase().as_str(), "txt" | "md" | "log");

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
}
