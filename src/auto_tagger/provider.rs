use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Errors that can occur during tag generation.
#[derive(Error, Debug, Clone)]
pub enum TagError {
    /// Authentication failed — invalid or missing API key.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The provider is temporarily unavailable (5xx, timeout, network error).
    #[error("provider unavailable: {0}")]
    Unavailable(String),

    /// The response could not be parsed as valid JSON.
    #[error("invalid response format: {0}")]
    Parse(String),

    /// The request was malformed (4xx, bad parameters).
    #[error("bad request: {0}")]
    BadRequest(String),
}

/// Structured entities extracted from document content.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Entities {
    /// Person names found in the document.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub persons: Vec<String>,
    /// Organization names found in the document.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub organizations: Vec<String>,
    /// Years referenced in the document (as strings, e.g., "2023").
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub years: Vec<String>,
    /// Document/form identifiers (e.g., "1040", "W-2").
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub doc_id: Vec<String>,
    /// Monetary amounts with currency (e.g., "$45,000", "EUR 1200").
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub amounts: Vec<String>,
}

/// Deserialize a value that could be either a single string or an array of strings.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;
    struct StringOrVec;
    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of strings")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                v.push(item);
            }
            Ok(v)
        }
    }
    deserializer.deserialize_any(StringOrVec)
}

/// The complete response from a tag provider: topic tags + extracted entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagResponse {
    /// Topic/type classification tags (e.g., "tax-return", "research-paper").
    pub tags: Vec<String>,
    /// Structured entities extracted from the document.
    #[serde(default)]
    pub entities: Entities,
}

/// A single tag suggestion with confidence metadata for UI display.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TagSuggestion {
    /// The tag name.
    pub tag: String,
    /// Confidence score from 0.0 to 1.0.
    pub confidence: f32,
    /// Whether this tag matches an existing tag in the user's vocabulary.
    pub is_existing: bool,
}

impl TagResponse {
    /// Convert this response into a flat list of TagSuggestions for UI rendering.
    #[allow(dead_code)]
    pub fn to_suggestions(
        &self,
        existing_vocabulary: &std::collections::HashSet<String>,
    ) -> Vec<TagSuggestion> {
        self.tags
            .iter()
            .map(|t| TagSuggestion {
                tag: t.clone(),
                confidence: 0.8,
                is_existing: existing_vocabulary.contains(t),
            })
            .collect()
    }
}

/// Trait for pluggable tag generation providers.
pub trait TagProvider: Send + Sync {
    /// Generate tags and extract entities from document text.
    ///
    /// `filename` is the document's filename (strongest signal for classification).
    /// `text` is the extracted text (first 3 pages, max ~2000 words).
    /// `existing_tags` is the user's current tag vocabulary for anchoring.
    fn generate_tags(
        &self,
        filename: &str,
        text: &str,
        existing_tags: &[String],
    ) -> Result<TagResponse, TagError>;
}

#[cfg(any(test, feature = "test-util"))]
mod tests {
    use super::*;

    /// A mock provider for testing that returns fixture data.
    #[derive(Clone)]
    #[allow(dead_code)]
    pub struct MockProvider {
        pub response: TagResponse,
        pub should_fail: bool,
        pub fail_error: Option<TagError>,
        pub call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[allow(dead_code)]
    impl MockProvider {
        pub fn new(response: TagResponse) -> Self {
            Self {
                response,
                should_fail: false,
                fail_error: None,
                call_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        pub fn failing(error: TagError) -> Self {
            Self {
                response: TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                },
                should_fail: true,
                fail_error: Some(error),
                call_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl TagProvider for MockProvider {
        fn generate_tags(
            &self,
            _filename: &str,
            _text: &str,
            _existing_tags: &[String],
        ) -> Result<TagResponse, TagError> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.should_fail {
                Err(self.fail_error.clone().unwrap())
            } else {
                Ok(self.response.clone())
            }
        }
    }

    #[test]
    fn mock_provider_returns_fixture_data() {
        let response = TagResponse {
            tags: vec!["tax".into(), "return".into()],
            entities: Entities {
                persons: vec!["Yang Guorui".into()],
                years: vec!["2023".into()],
                ..Default::default()
            },
        };
        let provider = MockProvider::new(response.clone());

        let result = provider
            .generate_tags("test.pdf", "sample text", &[])
            .unwrap();

        assert_eq!(result.tags, response.tags);
        assert_eq!(result.entities.persons, response.entities.persons);
        assert_eq!(result.entities.years, response.entities.years);
    }

    #[test]
    fn mock_provider_tracks_call_count() {
        let response = TagResponse {
            tags: vec!["test".into()],
            entities: Entities::default(),
        };
        let provider = MockProvider::new(response);

        provider.generate_tags("a.pdf", "text", &[]).unwrap();
        provider.generate_tags("b.pdf", "text", &[]).unwrap();

        assert_eq!(
            provider
                .call_count
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn mock_provider_returns_error_when_failing() {
        let provider = MockProvider::failing(TagError::Unavailable("test error".into()));

        let result = provider.generate_tags("test.pdf", "text", &[]);
        assert!(result.is_err());
        match result {
            Err(TagError::Unavailable(msg)) => assert!(msg.contains("test error")),
            _ => panic!("expected Unavailable error"),
        }
    }

    #[test]
    fn tag_suggestion_flags_existing_vocabulary() {
        let response = TagResponse {
            tags: vec!["tax".into(), "new-topic".into()],
            entities: Entities::default(),
        };
        let mut vocab = std::collections::HashSet::new();
        vocab.insert("tax".to_string());

        let suggestions = response.to_suggestions(&vocab);
        assert_eq!(suggestions.len(), 2);

        let tax_suggestion = suggestions.iter().find(|s| s.tag == "tax").unwrap();
        assert!(
            tax_suggestion.is_existing,
            "'tax' is in vocabulary, should be flagged"
        );

        let new_suggestion = suggestions.iter().find(|s| s.tag == "new-topic").unwrap();
        assert!(
            !new_suggestion.is_existing,
            "'new-topic' is not in vocabulary"
        );
    }

    #[test]
    fn tag_suggestion_confidence_in_range() {
        let response = TagResponse {
            tags: vec!["test".into()],
            entities: Entities::default(),
        };
        let suggestions = response.to_suggestions(&std::collections::HashSet::new());
        for s in &suggestions {
            assert!(s.confidence >= 0.0 && s.confidence <= 1.0);
        }
    }
}
