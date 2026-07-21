use serde::Serialize;

/// A search request with query string and options.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub tag_filters: Vec<String>,
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
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tag_filters = tags;
        self
    }

    /// Enable fuzzy matching.
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
    pub content_hash: String,
    pub tags: Vec<String>,
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
}
