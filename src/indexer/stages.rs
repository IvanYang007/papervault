use crate::indexer::extractors::ExtractedContent;

/// Assemble the extraction pipeline stages in priority order.
/// Returns a list of extractors — the first one that returns `Ok(Some(...))` wins.
pub fn create_extractor_chain() -> Vec<Box<dyn super::extractors::Extractor>> {
    let mut stages: Vec<Box<dyn super::extractors::Extractor>> = Vec::new();

    // PDF extractor first (most common file type)
    match super::extractors::pdf::PdfExtractor::new() {
        Ok(extractor) => stages.push(Box::new(extractor)),
        Err(e) => {
            tracing::error!("Failed to initialize PDF extractor: {}", e);
            // Continue without PDF support — text files will still work
        }
    }

    // Text extractor handles .txt, .md, .log
    stages.push(Box::new(super::extractors::text::TextExtractor));

    stages
}

/// Run the extractor chain on a file path.
/// Returns the extracted content from the first extractor that handles this file.
pub fn run_chain(
    path: &std::path::Path,
    stages: &[Box<dyn super::extractors::Extractor>],
) -> anyhow::Result<Option<ExtractedContent>> {
    for stage in stages {
        match stage.extract(path) {
            Ok(Some(content)) => return Ok(Some(content)),
            Ok(None) => continue, // This extractor doesn't handle this file type
            Err(e) => {
                tracing::warn!("Extractor error for {}: {}", path.display(), e);
                return Err(e);
            }
        }
    }
    Ok(None) // No extractor handled this file
}
