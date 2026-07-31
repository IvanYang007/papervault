use serde::Serialize;
use std::sync::Arc;

/// A search request with query string and options.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub tag_filters: Vec<String>,
    #[allow(dead_code)]
    pub fuzzy: bool,
    pub limit: usize,
}

impl SearchRequest {
    /// Create a new search request with defaults (no tag filters, exact match, limit 50).
    pub fn new(query: String) -> Self {
        Self {
            query,
            tag_filters: Vec::new(),
            fuzzy: false,
            limit: 50,
        }
    }

    /// Set tag filters for this request.
    #[allow(dead_code)]
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tag_filters = tags;
        self
    }

    /// Enable fuzzy matching.
    #[allow(dead_code)]
    pub fn with_fuzzy(mut self, fuzzy: bool) -> Self {
        self.fuzzy = fuzzy;
        self
    }

    /// Set result limit.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// A single search result.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub file_name: String,
    pub file_path: String,
    pub file_type: String,
    pub snippet: String,
    pub match_count: usize,
    #[serde(skip)]
    pub match_terms: Arc<[String]>,
    /// Pre-computed highlight spans into `snippet` (byte offsets, merged,
    /// char-boundary validated) — the render loop must not re-scan per frame.
    #[serde(skip)]
    pub highlight_spans: Vec<(usize, usize)>,
    pub content_hash: String,
    pub tags: Vec<String>,
    pub lower_snippet: String,
}

/// Find all match positions of the (lowercased) terms in a snippet, merge
/// overlapping spans, and validate UTF-8 boundaries against the original.
/// Byte offsets come from the lowercased string, which can misalign with the
/// original (e.g. Turkish İ → i̇) — misaligned spans are dropped.
pub fn compute_highlight_spans(
    snippet: &str,
    lower_snippet: &str,
    match_terms: &[String],
) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for term in match_terms {
        if term.is_empty() {
            continue;
        }
        let mut search_start = 0;
        while let Some(pos) = lower_snippet[search_start..].find(term) {
            let abs_start = search_start + pos;
            let abs_end = abs_start + term.len();
            spans.push((abs_start, abs_end));
            search_start = abs_end;
        }
    }
    if spans.is_empty() {
        return spans;
    }
    // Sort and merge overlapping spans, validating against the original
    // snippet's char boundaries.
    spans.sort_by_key(|s| s.0);
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for span in spans {
        if !snippet.is_char_boundary(span.0) || !snippet.is_char_boundary(span.1) {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if span.0 <= last.1 {
                last.1 = last.1.max(span.1);
            } else {
                merged.push(span);
            }
        } else {
            merged.push(span);
        }
    }
    merged
}

/// Collection of search results with metadata.
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub items: Vec<SearchResult>,
    pub total_hits: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_request_default_limit_is_50() {
        let req = SearchRequest::new("test".into());
        assert_eq!(req.limit, 50);
        assert!(req.tag_filters.is_empty());
        assert!(!req.fuzzy);
    }

    #[test]
    fn search_request_with_tag_filters() {
        let req = SearchRequest::new("test".into()).with_tags(vec!["tax".into(), "2025".into()]);
        assert_eq!(req.tag_filters.len(), 2);
        assert_eq!(req.tag_filters[0], "tax");
    }

    #[test]
    fn search_request_with_limit() {
        let req = SearchRequest::new("test".into()).with_limit(10);
        assert_eq!(req.limit, 10);
    }

    #[test]
    fn parse_single_term_builds_term_query() {
        use crate::search::engine::search_with_reader;
        use crate::search::schema::{build_schema, SchemaFields};
        use tantivy::doc;
        use tantivy::tokenizer::*;
        use tantivy::{Index, IndexReader, ReloadPolicy};

        let dir = tempfile::TempDir::new().unwrap();
        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);
        let index = Index::create_in_dir(dir.path(), schema.clone()).unwrap();

        let tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("body", tokenizer);

        let mut writer = index.writer(50_000_000).unwrap();
        writer
            .add_document(doc!(
                fields.doc_id => "hash1pdf",
                fields.file_path => "/test/doc.pdf",
                fields.file_name => "doc.pdf",
                fields.body => "hello world invoice march",
                fields.file_type => "pdf",
                fields.modified_ts => 1700000000i64,
                fields.content_hash => "hash1",
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        reader.reload().unwrap();

        let request = SearchRequest::new("invoice".into());
        let results = search_with_reader(&fields, &reader, &request).unwrap();
        assert_eq!(
            results.total_hits, 1,
            "Single term 'invoice' should find 1 doc"
        );
        assert!(results.items[0].file_name.contains("doc.pdf"));
    }

    #[test]
    fn empty_query_returns_empty_results() {
        use crate::search::engine::search_with_reader;
        use crate::search::schema::{build_schema, SchemaFields};
        use tantivy::doc;
        use tantivy::tokenizer::*;
        use tantivy::{Index, IndexReader, ReloadPolicy};

        let dir = tempfile::TempDir::new().unwrap();
        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);
        let index = Index::create_in_dir(dir.path(), schema.clone()).unwrap();

        let tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("body", tokenizer);

        let mut writer = index.writer(50_000_000).unwrap();
        writer
            .add_document(doc!(
                fields.doc_id => "hash1pdf",
                fields.file_path => "/test/doc.pdf",
                fields.file_name => "doc.pdf",
                fields.body => "hello world",
                fields.file_type => "pdf",
                fields.modified_ts => 1700000000i64,
                fields.content_hash => "hash1",
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        reader.reload().unwrap();

        // Empty string should return empty results without error
        let results = search_with_reader(&fields, &reader, &SearchRequest::new("".into())).unwrap();
        assert!(results.items.is_empty());
        assert_eq!(results.total_hits, 0);

        // Whitespace-only should also return empty
        let results =
            search_with_reader(&fields, &reader, &SearchRequest::new("   ".into())).unwrap();
        assert!(results.items.is_empty());
        assert_eq!(results.total_hits, 0);
    }

    // ── compute_highlight_spans tests ──

    #[test]
    fn highlight_spans_find_case_insensitive_matches() {
        let snippet = "The quick brown fox";
        let spans = compute_highlight_spans(snippet, &snippet.to_lowercase(), &["quick".into()]);
        assert_eq!(spans, vec![(4, 9)], "must find the term in lowercase text");
        assert_eq!(&snippet[4..9], "quick");
    }

    #[test]
    fn highlight_spans_merge_overlapping_terms() {
        let snippet = "tax return 2023";
        let lower = snippet.to_lowercase();
        let spans = compute_highlight_spans(snippet, &lower, &["tax".into(), "tax return".into()]);
        // (0,3) and (0,10) overlap -> merged to (0,10)
        assert_eq!(spans, vec![(0, 10)]);
    }

    #[test]
    fn highlight_spans_skip_non_char_boundaries() {
        // The lowercased offset misaligns with the original for İ (2-byte
        // uppercase, 3-byte lowercase) — the span must be dropped, not panic.
        let snippet = "İstanbul";
        let lower = snippet.to_lowercase();
        let spans = compute_highlight_spans(snippet, &lower, &["istanbul".into()]);
        assert!(
            spans.is_empty(),
            "misaligned spans must be dropped: {:?}",
            spans
        );
    }

    #[test]
    fn highlight_spans_no_terms_returns_empty() {
        let snippet = "plain text";
        let spans = compute_highlight_spans(snippet, &snippet.to_lowercase(), &[]);
        assert!(spans.is_empty());
    }
}
