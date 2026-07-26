use std::time::Duration;

use serde_json::Value;

use super::provider::{TagError, TagProvider, TagResponse};

/// Prompt template for the DeepSeek API — kept minimal to force JSON compliance.
/// Prompt template — minimal to force JSON compliance with deepseek-v4-flash.
const PROMPT_TEMPLATE: &str = r#"Output ONLY this JSON. No other text.
If text is empty or unextractable, generate tags from the filename alone.
IMPORTANT for person names: extract ALL name variants (full, partial, initials). Names may appear with/without spaces, reversed order, or CamelCase.
{"tags":["keyword1","keyword2"],"entities":{"persons":["Full Name"],"organizations":["Org Name"],"years":["2024"],"doc_id":["form-number"],"amounts":["$1,000"]}}

Filename: {{FILENAME}}
Text (first 2 pages): {{TEXT}}
Existing tags: {{EXISTING_TAGS}}
JSON:"#;

/// Provider that calls the DeepSeek API for tag generation.
pub struct DeepSeekProvider {
    agent: ureq::Agent,
    endpoint: String,
    model: String,
    api_key_env: String,
    timeout: Duration,
    /// Maximum words of text to send (soft cap — truncates at word boundary).
    max_text_words: usize,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider.
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(timeout_secs))
                .build(),
            endpoint: endpoint.into(),
            model: model.into(),
            api_key_env: api_key_env.into(),
            timeout: Duration::from_secs(timeout_secs),
            max_text_words: 1500, // ~2 pages of text
        }
    }

    /// Read the API key from the environment variable at request time.
    fn api_key(&self) -> Result<String, TagError> {
        let key = std::env::var(&self.api_key_env).map_err(|_| {
            TagError::Auth(format!(
                "environment variable '{}' is not set. Set it to your DeepSeek API key.",
                self.api_key_env
            ))
        })?;
        if key.trim().is_empty() {
            return Err(TagError::Auth(format!(
                "environment variable '{}' is set but empty",
                self.api_key_env
            )));
        }
        Ok(key)
    }

    /// Truncate text to at most `max_text_words` words at a word boundary.
    fn truncate_text<'a>(&self, text: &'a str) -> &'a str {
        let mut word_end = text.len();
        for (word_count, word) in text.split_whitespace().enumerate() {
            if word_count >= self.max_text_words {
                word_end = word.as_ptr() as usize - text.as_ptr() as usize;
                break;
            }
        }
        let trimmed = text[..word_end].trim_end();
        let trim_len = trimmed.as_ptr() as usize - text.as_ptr() as usize + trimmed.len();
        &text[..trim_len]
    }

    /// Build the prompt with filename, text, and existing tags substituted.
    fn build_prompt(&self, filename: &str, text: &str, existing_tags: &[String]) -> String {
        let truncated = self.truncate_text(text);
        let existing = if existing_tags.is_empty() {
            "[]".to_string()
        } else {
            serde_json::to_string(existing_tags).unwrap_or_else(|_| "[]".to_string())
        };
        // Single-pass replacement to avoid multiple intermediate allocations
        PROMPT_TEMPLATE
            .replace("{{FILENAME}}", filename)
            .replace("{{TEXT}}", truncated)
            .replace("{{EXISTING_TAGS}}", &existing)
    }

    /// Parse the DeepSeek API response JSON into a TagResponse.
    fn parse_response(body: &str) -> Result<TagResponse, TagError> {
        let root: Value = serde_json::from_str(body)
            .map_err(|e| TagError::Parse(format!("invalid JSON from API: {}", e)))?;

        let content = root["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["message"]["content"].as_str())
            .ok_or_else(|| {
                TagError::Parse("API response missing 'choices[0].message.content'".into())
            })?;

        let tag_response: TagResponse = serde_json::from_str(content).map_err(|e| {
            TagError::Parse(format!(
                "failed to parse tag JSON from model output: {}. Raw content: {}",
                e,
                &content[..content.len().min(200)]
            ))
        })?;

        Ok(tag_response)
    }
}

impl TagProvider for DeepSeekProvider {
    fn generate_tags(
        &self,
        filename: &str,
        text: &str,
        existing_tags: &[String],
    ) -> Result<TagResponse, TagError> {
        let api_key = self.api_key()?;
        let prompt = self.build_prompt(filename, text, existing_tags);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "temperature": 0.1,
            "max_tokens": 4000,
            "stream": false
        });

        let body_str = serde_json::to_string(&body).map_err(|e| {
            TagError::BadRequest(format!("failed to serialize request body: {}", e))
        })?;

        let response = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", api_key))
            .set("Content-Type", "application/json")
            .timeout(self.timeout)
            .send_string(&body_str)
            .map_err(|e| match &e {
                ureq::Error::Status(401 | 403, _) => {
                    TagError::Auth(format!("DeepSeek API auth failed: {}", e))
                }
                ureq::Error::Status(429, _) => {
                    TagError::Unavailable(format!("DeepSeek API rate limited: {}", e))
                }
                ureq::Error::Status(code, _) if *code >= 400 && *code < 500 => {
                    TagError::BadRequest(format!("DeepSeek API bad request ({}): {}", code, e))
                }
                ureq::Error::Status(_, _) => {
                    TagError::Unavailable(format!("DeepSeek API server error: {}", e))
                }
                ureq::Error::Transport(_) => {
                    TagError::Unavailable(format!("DeepSeek API unreachable: {}", e))
                }
            })?;

        let body_str = response.into_string().map_err(|e| {
            TagError::Unavailable(format!("failed to read API response body: {}", e))
        })?;

        Self::parse_response(&body_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider() -> DeepSeekProvider {
        DeepSeekProvider::new(
            "https://api.deepseek.com/v1/chat/completions",
            "deepseek-chat",
            "DEEPSEEK_API_KEY",
            30,
        )
    }

    #[test]
    fn truncate_text_under_limit_returns_full_text() {
        let provider = make_provider();
        let text = "hello world";
        assert_eq!(provider.truncate_text(text), "hello world");
    }

    #[test]
    fn truncate_text_at_word_boundary() {
        let provider = make_provider();
        let words: Vec<String> = (0..1505).map(|i| format!("word{}", i)).collect();
        let text = words.join(" ");
        let truncated = provider.truncate_text(&text);

        let word_count = truncated.split_whitespace().count();
        assert_eq!(word_count, 1500);
        assert!(!truncated.ends_with(' '));
    }

    #[test]
    fn truncate_text_empty_string() {
        let provider = make_provider();
        assert_eq!(provider.truncate_text(""), "");
    }

    #[test]
    fn build_prompt_includes_filename_and_truncated_text() {
        let provider = make_provider();
        let prompt = provider.build_prompt(
            "2023-tax-return.pdf",
            "Form 1040 tax document content here",
            &["tax".into(), "irs".into()],
        );

        assert!(prompt.contains("2023-tax-return.pdf"));
        assert!(prompt.contains("Form 1040 tax document content here"));
        assert!(prompt.contains(r#""tax""#));
        assert!(prompt.contains(r#""irs""#));
        // Verify no unreplaced placeholders remain
        assert!(
            !prompt.contains("{existing_tags"),
            "placeholder not replaced"
        );
        assert!(!prompt.contains("{{FILENAME}}"), "FILENAME not replaced");
    }

    #[test]
    fn truncate_text_handles_consecutive_whitespace() {
        let provider = make_provider();
        let text = "a   b   c   d   e";
        assert_eq!(provider.truncate_text(text), "a   b   c   d   e");
    }

    #[test]
    fn build_prompt_empty_existing_tags_uses_empty_array() {
        let provider = make_provider();
        let prompt = provider.build_prompt("test.pdf", "content", &[]);
        assert!(prompt.contains("[]"), "empty tags should produce '[]'");
    }

    #[test]
    fn parse_response_extracts_tags_and_entities() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "{\"tags\": [\"tax-return\", \"tax\"], \"entities\": {\"persons\": [\"Yang Guorui\"], \"years\": [\"2023\"], \"organizations\": [], \"doc_id\": [], \"amounts\": []}}"
                }
            }]
        }"#;

        let result = DeepSeekProvider::parse_response(body).unwrap();

        assert_eq!(result.tags, vec!["tax-return", "tax"]);
        assert_eq!(result.entities.persons, vec!["Yang Guorui"]);
        assert_eq!(result.entities.years, vec!["2023"]);
    }

    #[test]
    fn parse_response_handles_empty_entities() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "{\"tags\": [\"test\"], \"entities\": {}}"
                }
            }]
        }"#;

        let result = DeepSeekProvider::parse_response(body).unwrap();

        assert_eq!(result.tags, vec!["test"]);
        assert!(result.entities.persons.is_empty());
    }

    #[test]
    fn parse_response_rejects_missing_choices() {
        let body = r#"{"error": "not found"}"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_response_rejects_invalid_json_in_content() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "not valid json {{{"
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(result.is_err());
    }
}
